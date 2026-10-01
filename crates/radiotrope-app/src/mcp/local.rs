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

use crate::instance::{self, Acquire, Instance, HELLO_MCP, HELLO_SHOW};

use super::tools::RadioTools;

/// How long the relay waits for a player it started to come up
const START_WAIT: Duration = Duration::from_secs(10);
/// After the agent closes stdin, how long the relay still waits for answers
/// to requests the player is working on
const DRAIN_LIMIT: Duration = Duration::from_secs(10);
/// How long a new connection has to say what it wants, and the player has
/// to answer the relay's hello
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
    let hello = hello.trim_end();
    // "radiotrope/1 mcp from=<pid>": the relay's parent, the agent app
    let mcp = hello.strip_prefix(HELLO_MCP).and_then(|rest| {
        if rest.is_empty() {
            return Some(None);
        }
        let pid = rest.strip_prefix(" from=")?;
        Some(pid.parse::<u32>().ok())
    });
    if let Some(app) = mcp {
        {
            if conn
                .write_all(format!("{WELCOME}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
            // Counts on the menu bar's agents chip while connected; the
            // sessions of one app count once
            let here = tools.presence().local_connected(app);
            match tools.for_local(here.place()).serve(conn).await {
                Ok(session) => {
                    let _ = session.waiting().await;
                }
                Err(e) => eprintln!("MCP: session failed: {e}"),
            }
        }
    } else if hello == HELLO_SHOW {
        show_window();
    }
}

/// The relay's first line: [`HELLO_MCP`], with the process that started the
/// relay where we know it. One app can open several sessions (Claude
/// Desktop's chat and its agent mode each start one), and they share it.
fn mcp_hello() -> String {
    match parent_pid() {
        Some(parent) => format!("{HELLO_MCP} from={parent}\n"),
        // Each session counts alone
        None => format!("{HELLO_MCP}\n"),
    }
}

/// The process that started this one
#[cfg(unix)]
fn parent_pid() -> Option<u32> {
    // SAFETY: getppid has no preconditions and cannot fail
    let parent = unsafe { libc::getppid() };
    u32::try_from(parent).ok()
}

/// The process that started this one, from the system's list of processes
/// (Windows has no direct call for it)
#[cfg(windows)]
fn parent_pid() -> Option<u32> {
    use std::ffi::c_void;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};

    /// PROCESSENTRY32W
    #[repr(C)]
    struct ProcessEntry {
        size: u32,
        usage: u32,
        process_id: u32,
        default_heap_id: usize,
        module_id: u32,
        threads: u32,
        parent_process_id: u32,
        priority_class_base: i32,
        flags: u32,
        exe_file: [u16; 260],
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut c_void;
        fn Process32FirstW(snapshot: *mut c_void, entry: *mut ProcessEntry) -> i32;
        fn Process32NextW(snapshot: *mut c_void, entry: *mut ProcessEntry) -> i32;
    }
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;

    // SAFETY: plain Win32 call; INVALID_HANDLE_VALUE (-1) means it failed
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot as isize == -1 {
        return None;
    }
    // SAFETY: a handle we just opened, closed when this drops
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let handle = std::os::windows::io::AsRawHandle::as_raw_handle(&snapshot);
    let ours = std::process::id();
    // SAFETY: all-zero is a valid PROCESSENTRY32W; the calls need its size
    // set and write nothing past it
    let mut entry: ProcessEntry = unsafe { std::mem::zeroed() };
    entry.size = std::mem::size_of::<ProcessEntry>() as u32;
    // SAFETY: a snapshot handle and an entry with its size set
    let mut more = unsafe { Process32FirstW(handle, &mut entry) } != 0;
    while more {
        if entry.process_id == ours {
            return Some(entry.parent_process_id).filter(|&pid| pid != 0);
        }
        // SAFETY: as above
        more = unsafe { Process32NextW(handle, &mut entry) } != 0;
    }
    None
}

#[cfg(not(any(unix, windows)))]
fn parent_pid() -> Option<u32> {
    None
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
    match instance::connect().await {
        Ok(conn) => return Ok(conn),
        // Another user's socket or pipe, or one we may not open: a player
        // we start couldn't serve agents either
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Err(e),
        Err(_) => {}
    }
    let started = start_player_if_none(instance::acquire())?;
    let deadline = Instant::now() + START_WAIT;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match instance::connect().await {
            Ok(conn) => return Ok(conn),
            Err(e) if Instant::now() >= deadline => {
                return Err(std::io::Error::other(if started {
                    // Most likely it could not open its window
                    format!(
                        "the player did not start within {} s ({e}); start Radiotrope \
                         from the desktop and try again",
                        START_WAIT.as_secs()
                    )
                } else {
                    format!(
                        "the player is running but does not answer agents ({e}); quit \
                         Radiotrope and try again"
                    )
                }));
            }
            Err(_) => {}
        }
    }
}

/// No player answers: start one if none holds the lock, and say whether we
/// did. One that holds it may still be starting up, so it is waited for
/// instead; a second player would only show its window, or, without the
/// lock, run beside it where agents can't reach it.
fn start_player_if_none(found: Acquire) -> std::io::Result<bool> {
    match found {
        Acquire::Primary(lock) => {
            // Let go, for the player to take
            drop(lock);
            instance::spawn_player()?;
            Ok(true)
        }
        Acquire::Running => Ok(false),
        Acquire::Unavailable(e) => Err(std::io::Error::new(
            e.kind(),
            format!("can't tell whether the player runs: {e}"),
        )),
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
    to_player.write_all(mcp_hello().as_bytes()).await?;
    let mut from_player = BufReader::new(from_player);
    let mut answer = String::new();
    // A hung player must not hang the agent
    let mut limited = (&mut from_player).take(HELLO_MAX);
    let read = limited.read_line(&mut answer);
    match tokio::time::timeout(HELLO_WAIT, read).await {
        Ok(Ok(_)) if answer.trim_end() == WELCOME => {}
        Ok(Err(e)) => return Err(e),
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
            {
                let mut waiting = open.borrow_mut();
                waiting.extend(request_ids(&line));
                // A request the agent gave up on may never be answered
                for id in cancelled_ids(&line) {
                    waiting.remove(&id);
                }
            }
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

/// The JSON-RPC messages of one line (a batch holds several); none for a
/// line that isn't JSON
fn messages(line: &str) -> Vec<serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(serde_json::Value::Array(batch)) => batch,
        Ok(single) => vec![single],
        Err(_) => Vec::new(),
    }
}

/// Ids of the requests one line from the agent cancels
/// (`notifications/cancelled`, whose `requestId` names the request)
fn cancelled_ids(line: &str) -> Vec<String> {
    messages(line)
        .iter()
        .filter(|msg| msg.get("method").and_then(|m| m.as_str()) == Some("notifications/cancelled"))
        .filter_map(|msg| msg.get("params")?.get("requestId"))
        .filter(|id| !id.is_null())
        .map(|id| id.to_string())
        .collect()
}

/// Ids answered by one line from the player
fn response_ids(line: &str) -> Vec<String> {
    message_ids(line, |msg| {
        msg.get("method").is_none() && (msg.get("result").is_some() || msg.get("error").is_some())
    })
}

fn message_ids(line: &str, wanted: impl Fn(&serde_json::Value) -> bool) -> Vec<String> {
    messages(line)
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
        assert_eq!(
            cancelled_ids(
                r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#
            ),
            vec!["7"]
        );
        assert_eq!(
            cancelled_ids(r#"[{"method":"notifications/cancelled","params":{"requestId":"a"}}]"#),
            vec![r#""a""#]
        );
        assert!(cancelled_ids(r#"{"id":7,"method":"tools/call"}"#).is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_request_is_not_waited_for() {
        let (relay_side, player_side) = duplex(4096);
        // A player that would answer only after a minute
        let _player = tokio::spawn(fake_player(player_side, Duration::from_secs(60)));
        let (mut agent_in, relay_in) = duplex(4096);
        let (relay_out, _agent_out) = duplex(4096);
        agent_in
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\"}\n\
                  {\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\
                  \"params\":{\"requestId\":1}}\n",
            )
            .await
            .unwrap();
        drop(agent_in);
        tokio::time::timeout(
            Duration::from_millis(500),
            relay(relay_in, relay_out, relay_side),
        )
        .await
        .expect("the relay waited for a cancelled request")
        .unwrap();
    }

    /// A fake player: checks the hello line, then answers each request
    /// after `delay`
    async fn fake_player(conn: tokio::io::DuplexStream, delay: Duration) {
        let (read, mut write) = tokio::io::split(conn);
        let mut read = BufReader::new(read);
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        // With the app that started the relay where known
        assert!(line.starts_with(HELLO_MCP), "{line}");
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

    #[test]
    fn a_player_is_started_only_when_none_holds_the_lock() {
        // One that holds the lock is waited for
        assert!(!start_player_if_none(Acquire::Running).unwrap());
        // Without the lock a player would run where agents can't reach it
        let unavailable = Acquire::Unavailable(std::io::ErrorKind::PermissionDenied.into());
        let e = start_player_if_none(unavailable).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn the_relay_gives_up_on_a_player_that_never_says_ok() {
        let (relay_side, mut player_side) = duplex(4096);
        let (_agent_in, relay_in) = duplex(64);
        let (relay_out, _agent_out) = duplex(64);
        // A long line with no end: the relay stops reading after a few bytes
        player_side.write_all(&[b'x'; 1024]).await.unwrap();
        let relayed = tokio::time::timeout(
            Duration::from_millis(500),
            relay(relay_in, relay_out, relay_side),
        )
        .await
        .expect("relay did not give up");
        assert!(relayed.is_err());
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
