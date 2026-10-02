//! Old SHOUTcast servers
//!
//! SHOUTcast v1 servers answer `ICY 200 OK` where HTTP says `HTTP/1.0 200 OK`,
//! and the HTTP client rejects that reply. [`get`] speaks just enough
//! HTTP/1.0 to play them: one request, a status line of either kind, headers,
//! then the body until the connection closes. These servers only speak plain
//! `http://`. The station's own address may redirect to one (the HTTP client
//! followed it before it failed), so redirects are followed too.
//!
//! The request goes over a plain TCP connection, so it ignores any proxy
//! the HTTP client would use.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, LOCATION};
use reqwest::{StatusCode, Url};

use crate::config::network::{CONNECT_TIMEOUT_SECS, READ_TIMEOUT_SECS, USER_AGENT};

/// Longest status line plus headers we read
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Most redirects followed before the old server is reached
const MAX_REDIRECTS: usize = 5;

/// A reply: its status and headers, and the connection to read its body from
pub(crate) struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: BufReader<TcpStream>,
}

/// Request `url` (with ICY metadata) and read the reply's status and
/// headers, following redirects
pub(crate) fn get(url: &str) -> io::Result<Reply> {
    let mut url = Url::parse(url).map_err(invalid)?;
    for _ in 0..=MAX_REDIRECTS {
        let reply = get_once(&url)?;
        match redirect(&reply, &url) {
            Some(next) => url = next,
            None => return Ok(reply),
        }
    }
    Err(invalid("too many redirects"))
}

/// Where a redirect reply sends the request
fn redirect(reply: &Reply, url: &Url) -> Option<Url> {
    if !matches!(reply.status.as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let location = reply.headers.get(LOCATION)?.to_str().ok()?;
    url.join(location.trim()).ok()
}

/// One request to `url`, without following a redirect
fn get_once(url: &Url) -> io::Result<Reply> {
    if url.scheme() != "http" {
        return Err(invalid("only plain http"));
    }
    // In brackets for an IPv6 address, as the Host header wants it
    let host = url.host_str().ok_or_else(|| invalid("no host"))?;

    let stream = connect(url)?;
    stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))?;
    stream.set_write_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))?;

    let target = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    let host_header = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let request = format!(
        "GET {target} HTTP/1.0\r\nHost: {host_header}\r\nUser-Agent: {USER_AGENT}\r\n\
         Accept: */*\r\nIcy-MetaData: 1\r\nConnection: close\r\n\r\n"
    );
    (&stream).write_all(request.as_bytes())?;

    let mut body = BufReader::new(stream);
    let status = parse_status(&read_line(&mut body, &mut 0)?)?;
    let mut headers = HeaderMap::new();
    let mut head_bytes = 0;
    loop {
        let line = read_line(&mut body, &mut head_bytes)?;
        if line.is_empty() {
            break;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let name = HeaderName::from_bytes(line[..colon].trim_ascii());
        let value = HeaderValue::from_bytes(line[colon + 1..].trim_ascii());
        if let (Ok(name), Ok(value)) = (name, value) {
            headers.append(name, value);
        }
    }
    Ok(Reply {
        status,
        headers,
        body,
    })
}

fn connect(url: &Url) -> io::Result<TcpStream> {
    let timeout = Duration::from_secs(CONNECT_TIMEOUT_SECS);
    let mut last_error = io::Error::new(io::ErrorKind::NotFound, "no address for the host");
    for addr in url.socket_addrs(|| Some(80))? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_error = e,
        }
    }
    Err(last_error)
}

/// `ICY 200 OK` or `HTTP/1.x 200 OK`
fn parse_status(line: &[u8]) -> io::Result<StatusCode> {
    let line = String::from_utf8_lossy(line);
    let mut parts = line.split_ascii_whitespace();
    let protocol = parts.next().unwrap_or("");
    if protocol != "ICY" && !protocol.starts_with("HTTP/") {
        return Err(invalid(format!("not an ICY or HTTP reply: {line}")));
    }
    parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .and_then(|code| StatusCode::from_u16(code).ok())
        .ok_or_else(|| invalid(format!("no status code: {line}")))
}

/// One header line without its line ending. `total` counts the bytes of
/// the head read so far, which is capped.
fn read_line(reader: &mut BufReader<TcpStream>, total: &mut usize) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let limit = (MAX_HEAD_BYTES - *total) as u64;
    let read = reader.by_ref().take(limit).read_until(b'\n', &mut line)?;
    *total += read;
    if !line.ends_with(b"\n") {
        return Err(invalid("the reply's headers are cut off or too long"));
    }
    line.pop();
    if line.ends_with(b"\r") {
        line.pop();
    }
    Ok(line)
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::test_server::{Route, TestServer};

    #[test]
    fn reads_an_icy_reply() {
        let server = TestServer::start();
        server.route(
            "/;stream.mp3",
            Route::new("audio bytes").raw_head(
                "ICY 200 OK\r\nicy-notice1: <BR>This stream requires Winamp<BR>\r\n\
                 icy-name:Old FM\r\nicy-metaint:8192\r\ncontent-type:audio/mpeg\r\n\r\n",
            ),
        );
        let mut reply = get(&server.url("/;stream.mp3")).unwrap();
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.headers["icy-name"], "Old FM");
        assert_eq!(reply.headers["icy-metaint"], "8192");
        assert_eq!(reply.headers["content-type"], "audio/mpeg");
        let mut body = String::new();
        reply.body.read_to_string(&mut body).unwrap();
        assert_eq!(body, "audio bytes");
    }

    #[test]
    fn reads_an_http_reply_and_its_status() {
        let server = TestServer::start();
        server.route(
            "/full",
            Route::new("").raw_head("ICY 401 Service Unavailable\r\n\r\n"),
        );
        assert_eq!(
            get(&server.url("/full")).unwrap().status,
            StatusCode::UNAUTHORIZED
        );
        server.route("/live", Route::new("x").header("icy-br", "128"));
        let reply = get(&server.url("/live")).unwrap();
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.headers["icy-br"], "128");
    }

    #[test]
    fn keeps_header_bytes_that_are_not_utf8() {
        let server = TestServer::start();
        let mut head = b"ICY 200 OK\r\nicy-name: ".to_vec();
        head.extend_from_slice(&[0xD1, 0xDC, 0xE4, 0xE9, 0xEF]); // Ράδιο in Windows-1253
        head.extend_from_slice(b"\r\n\r\n");
        server.route("/live", Route::new("x").raw_head(head));
        let reply = get(&server.url("/live")).unwrap();
        assert_eq!(
            reply.headers["icy-name"].as_bytes(),
            [0xD1, 0xDC, 0xE4, 0xE9, 0xEF]
        );
    }

    #[test]
    fn follows_redirects_to_the_old_server() {
        // The station's address redirects to its SHOUTcast v1 server. The
        // HTTP client follows, then fails on the ICY reply; asking the
        // station's address again used to stop at the redirect.
        let server = TestServer::start();
        server.route("/station", Route::redirect("/relay?id=7"));
        server.route("/relay?id=7", Route::redirect(&server.url("/;stream.mp3")));
        server.route(
            "/;stream.mp3",
            Route::new("audio bytes").raw_head("ICY 200 OK\r\nicy-name:Old FM\r\n\r\n"),
        );
        let mut reply = get(&server.url("/station")).unwrap();
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.headers["icy-name"], "Old FM");
        let mut body = String::new();
        reply.body.read_to_string(&mut body).unwrap();
        assert_eq!(body, "audio bytes");
    }

    #[test]
    fn gives_up_on_redirects_it_cannot_follow() {
        let server = TestServer::start();
        server.route("/a", Route::redirect("/b"));
        server.route("/b", Route::redirect("/a"));
        assert!(get(&server.url("/a")).is_err(), "a loop");
        server.route("/tls", Route::redirect("https://radio.example/live"));
        assert!(get(&server.url("/tls")).is_err(), "no TLS");
        // A redirect without a Location is the reply
        server.route("/odd", Route::status(302));
        assert_eq!(get(&server.url("/odd")).unwrap().status, StatusCode::FOUND);
    }

    #[test]
    fn reaches_a_server_by_its_ipv6_address() {
        // `[::1]` was looked up as a host name. Skipped without IPv6.
        let Ok(server) = TestServer::bind("[::1]:0") else {
            return;
        };
        server.route(
            "/;stream.mp3",
            Route::new("audio bytes").raw_head("ICY 200 OK\r\nicy-name:Old FM\r\n\r\n"),
        );
        let url = server.url("/;stream.mp3");
        assert!(url.starts_with("http://[::1]:"), "{url}");
        let reply = get(&url).unwrap();
        assert_eq!(reply.headers["icy-name"], "Old FM");
    }

    #[test]
    fn rejects_what_is_not_an_icy_or_http_reply() {
        let server = TestServer::start();
        server.route("/junk", Route::new("").raw_head("SSH-2.0-OpenSSH\r\n\r\n"));
        assert!(get(&server.url("/junk")).is_err());
        assert!(get("https://example.com/stream").is_err(), "no TLS");
    }
}
