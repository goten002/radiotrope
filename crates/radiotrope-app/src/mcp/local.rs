//! Local MCP: every agent on this computer shares the one player
//!
//! `radiotrope --mcp` is a relay. It connects to the running player over the
//! local socket (starting the player first if it isn't running) and copies
//! the agent's stdin and stdout to and from it. The player serves each
//! connection as its own MCP session, so several agents, even on different
//! protocol versions, can use it at once.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use interprocess::local_socket::tokio::prelude::*;
use rmcp::ServiceExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::instance::{self, Instance, HELLO_MCP, HELLO_SHOW};

use super::tools::RadioTools;

/// How long the relay waits for a player it started to come up
const START_WAIT: Duration = Duration::from_secs(10);
/// After the agent closes stdin, how long the relay still waits for answers
/// to requests the player is working on
const DRAIN_LIMIT: Duration = Duration::from_secs(10);
/// How long a new connection has to say what it wants
const HELLO_WAIT: Duration = Duration::from_secs(5);
/// Longest first line we read before giving up on a connection
const HELLO_MAX: u64 = 64;

/// The player's answer to [`HELLO_MCP`]: go ahead
const WELCOME: &str = "ok";

/// Called when a second launch asks the player to show its window
pub type ShowWindow = Arc<dyn Fn() + Send + Sync>;

/// Serve MCP sessions and show requests on the local socket (blocking: call
/// from a dedicated thread)
pub fn serve(instance: &Instance, tools: RadioTools, show_window: ShowWindow) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("MCP: failed to start: {e}");
            return;
        }
    };
    runtime.block_on(async {
        let listener = match instance.listen() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("MCP: cannot listen for agents: {e}");
                return;
            }
        };
        loop {
            match listener.accept().await {
                Ok(conn) => {
                    tokio::spawn(handle(conn, tools.clone(), show_window.clone()));
                }
                Err(e) => {
                    eprintln!("MCP: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
}

/// One connection: read what it wants, then serve it
async fn handle<S>(conn: S, tools: RadioTools, show_window: ShowWindow)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut conn = BufReader::new(conn);
    let mut hello = String::new();
    let mut limited = (&mut conn).take(HELLO_MAX);
    let read = limited.read_line(&mut hello);
    if !matches!(tokio::time::timeout(HELLO_WAIT, read).await, Ok(Ok(_))) {
        return;
    }
    match hello.trim_end() {
        HELLO_MCP => {
            if conn
                .write_all(format!("{WELCOME}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
            // Counts on the menu bar's agents chip while connected
            let here = tools.presence().local_connected();
            match tools.for_local(here.place()).serve(conn).await {
                Ok(session) => {
                    let _ = session.waiting().await;
                }
                Err(e) => eprintln!("MCP: session failed: {e}"),
            }
        }
        HELLO_SHOW => show_window(),
        _ => {}
    }
}

/// `radiotrope --mcp`: relay stdin and stdout to the running player. Returns
/// the process exit code.
pub fn run_relay() -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("radiotrope: {e}");
            return 1;
        }
    };
    let code = runtime.block_on(async {
        let conn = match connect_or_start().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("radiotrope: cannot reach the player: {e}");
                return 1;
            }
        };
        match relay(tokio::io::stdin(), tokio::io::stdout(), conn).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("radiotrope: {e}");
                1
            }
        }
    });
    // The stdin reader is a blocking thread that only ends when the agent
    // closes stdin: don't wait for it when the player went away first
    runtime.shutdown_background();
    code
}

async fn connect_or_start() -> std::io::Result<interprocess::local_socket::tokio::Stream> {
    if let Ok(conn) = instance::connect().await {
        return Ok(conn);
    }
    instance::spawn_player()?;
    let deadline = Instant::now() + START_WAIT;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match instance::connect().await {
            Ok(conn) => return Ok(conn),
            // Most likely it could not open its window
            Err(e) if Instant::now() >= deadline => {
                return Err(std::io::Error::other(format!(
                    "the player did not start within {} s ({e}); start Radiotrope \
                     from the desktop and try again",
                    START_WAIT.as_secs()
                )))
            }
            Err(_) => {}
        }
    }
}

/// Copy the agent's messages to the player and the player's back, one line
/// (one JSON-RPC message) at a time.
///
/// Neither a Unix socket here nor a Windows pipe can pass on "no more
/// input" while staying open for answers, so the relay keeps count instead:
/// once the agent closes its input, it waits for the answers to the requests
/// still open, then closes the connection and returns.
async fn relay<I, O, S>(input: I, output: O, conn: S) -> std::io::Result<()>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (from_player, mut to_player) = tokio::io::split(conn);
    to_player
        .write_all(format!("{HELLO_MCP}\n").as_bytes())
        .await?;
    let mut from_player = BufReader::new(from_player);
    let mut answer = String::new();
    from_player.read_line(&mut answer).await?;
    match answer.trim_end() {
        WELCOME => {}
        _ => return Err(std::io::Error::other("the player did not answer")),
    }

    let open: RefCell<HashSet<String>> = RefCell::default();
    let input_closed = Cell::new(false);

    let up = async {
        let mut input = BufReader::new(input);
        let mut line = String::new();
        loop {
            line.clear();
            if input.read_line(&mut line).await? == 0 {
                return Ok::<_, std::io::Error>(());
            }
            open.borrow_mut().extend(request_ids(&line));
            to_player.write_all(line.as_bytes()).await?;
        }
    };

    let down = async {
        let mut output = output;
        let mut line = String::new();
        loop {
            line.clear();
            if from_player.read_line(&mut line).await? == 0 {
                return Ok::<_, std::io::Error>(());
            }
            output.write_all(line.as_bytes()).await?;
            output.flush().await?;
            let mut waiting = open.borrow_mut();
            for id in response_ids(&line) {
                waiting.remove(&id);
            }
            if input_closed.get() && waiting.is_empty() {
                return Ok(());
            }
        }
    };

    tokio::pin!(down);
    tokio::select! {
        result = up => {
            result?;
            input_closed.set(true);
            if !open.borrow().is_empty() {
                let _ = tokio::time::timeout(DRAIN_LIMIT, &mut down).await;
            }
        }
        // The player went away (its window was closed)
        result = &mut down => result?,
    }
    Ok(())
}

/// Ids of the requests in one line from the agent (a batch holds several)
fn request_ids(line: &str) -> Vec<String> {
    message_ids(line, |msg| msg.get("method").is_some())
}

/// Ids answered by one line from the player
fn response_ids(line: &str) -> Vec<String> {
    message_ids(line, |msg| {
        msg.get("method").is_none() && (msg.get("result").is_some() || msg.get("error").is_some())
    })
}

fn message_ids(line: &str, wanted: impl Fn(&serde_json::Value) -> bool) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let messages = match value {
        serde_json::Value::Array(batch) => batch,
        single => vec![single],
    };
    messages
        .iter()
        .filter(|msg| wanted(msg))
        .filter_map(|msg| msg.get("id"))
        .filter(|id| !id.is_null())
        .map(|id| id.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::duplex;

    #[test]
    fn requests_and_answers_are_matched_by_id() {
        assert_eq!(
            request_ids(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#),
            vec!["7"]
        );
        assert_eq!(
            request_ids(r#"{"jsonrpc":"2.0","id":"a","method":"ping"}"#),
            vec![r#""a""#]
        );
        // Notifications and answers from the agent are not waited for
        assert!(
            request_ids(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_empty()
        );
        assert!(request_ids(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#).is_empty());
        assert_eq!(
            request_ids(r#"[{"id":1,"method":"a"},{"id":2,"method":"b"}]"#),
            vec!["1", "2"]
        );
        assert_eq!(
            response_ids(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#),
            vec!["7"]
        );
        assert_eq!(response_ids(r#"{"id":7,"error":{"code":-1}}"#), vec!["7"]);
        // A request from the player is not an answer
        assert!(response_ids(r#"{"id":7,"method":"roots/list"}"#).is_empty());
        assert!(response_ids("not json").is_empty());
    }

    /// A fake player: checks the hello line, then answers each request
    /// after `delay`
    async fn fake_player(conn: tokio::io::DuplexStream, delay: Duration) {
        let (read, mut write) = tokio::io::split(conn);
        let mut read = BufReader::new(read);
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim_end(), HELLO_MCP);
        write.write_all(b"ok\n").await.unwrap();
        loop {
            line.clear();
            if read.read_line(&mut line).await.unwrap() == 0 {
                return;
            }
            for id in request_ids(&line) {
                tokio::time::sleep(delay).await;
                let answer = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}\n");
                if write.write_all(answer.as_bytes()).await.is_err() {
                    return;
                }
            }
        }
    }

    #[tokio::test]
    async fn the_relay_waits_for_open_answers_then_exits() {
        let (relay_side, player_side) = duplex(4096);
        let player = tokio::spawn(fake_player(player_side, Duration::from_millis(200)));
        let (mut agent_in, relay_in) = duplex(4096);
        let (relay_out, mut agent_out) = duplex(4096);

        agent_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\"}\n")
            .await
            .unwrap();
        // The agent closes its input before the answer is ready
        drop(agent_in);

        let started = Instant::now();
        tokio::time::timeout(
            Duration::from_secs(5),
            relay(relay_in, relay_out, relay_side),
        )
        .await
        .expect("relay did not exit")
        .unwrap();
        assert!(started.elapsed() < DRAIN_LIMIT);

        let mut answer = String::new();
        agent_out.read_to_string(&mut answer).await.unwrap();
        assert!(answer.contains("\"id\":1"), "{answer}");
        player.await.unwrap();
    }

    #[tokio::test]
    async fn the_relay_exits_at_once_with_nothing_open() {
        let (relay_side, player_side) = duplex(4096);
        // A player that would hold the connection open forever
        let _player = tokio::spawn(fake_player(player_side, Duration::from_secs(60)));
        let (agent_in, relay_in) = duplex(64);
        let (relay_out, _agent_out) = duplex(64);
        drop(agent_in);
        tokio::time::timeout(
            Duration::from_millis(500),
            relay(relay_in, relay_out, relay_side),
        )
        .await
        .expect("relay did not exit")
        .unwrap();
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

    #[tokio::test]
    async fn a_show_request_shows_the_window() {
        let shown = Arc::new(AtomicUsize::new(0));
        let counter = shown.clone();
        let show: ShowWindow = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let (mut client, server) = duplex(256);
        client
            .write_all(format!("{HELLO_SHOW}\n").as_bytes())
            .await
            .unwrap();
        handle(server, test_tools(), show).await;
        assert_eq!(shown.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unknown_hello_is_dropped() {
        let shown = Arc::new(AtomicUsize::new(0));
        let counter = shown.clone();
        let show: ShowWindow = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let (mut client, server) = duplex(256);
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        handle(server, test_tools(), show).await;
        assert_eq!(shown.load(Ordering::SeqCst), 0);
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn an_mcp_hello_starts_a_session() {
        let show: ShowWindow = Arc::new(|| {});
        let (client, server) = duplex(64 * 1024);
        let session = tokio::spawn(handle(server, test_tools(), show));
        let (read, mut write) = tokio::io::split(client);
        let mut read = BufReader::new(read);
        write
            .write_all(
                format!(
                    "{HELLO_MCP}\n{}\n",
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        assert_eq!(line, "ok\n");
        line.clear();
        read.read_line(&mut line).await.unwrap();
        assert!(line.contains("\"radiotrope\""), "{line}");
        drop(write);
        drop(read);
        tokio::time::timeout(Duration::from_secs(5), session)
            .await
            .expect("session did not end")
            .unwrap();
    }
}
