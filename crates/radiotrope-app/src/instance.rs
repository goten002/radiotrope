//! One player per user
//!
//! The first radiotrope to start takes an exclusive lock on `instance.lock`
//! and listens on a local socket. Everything else reaches that player through
//! the socket: `radiotrope --mcp` relays an agent's MCP session to it, and a
//! second launch asks it to show its window, then exits. The OS drops the lock
//! when the player exits or crashes, so a stale lock never blocks a restart.
//!
//! - Linux: `$XDG_RUNTIME_DIR/radiotrope/mcp.sock`, in a folder only the user
//!   can open (0700). Without `XDG_RUNTIME_DIR` (some agents, Claude Desktop
//!   among them, start `--mcp` with a trimmed environment), systemd's
//!   `/run/user/<uid>`, which is where it points anyway; failing that, a 0700
//!   folder in the temp directory. Abstract socket names are avoided: they
//!   have no permissions.
//! - Windows: the named pipe `\\.\pipe\radiotrope-<user SID>`, which
//!   refuses remote clients and is open to that user only. Anyone can make a
//!   pipe of any name, so a client talks only to a pipe whose server runs as
//!   its own user. The lock file lives in the local app data folder.
//!
//! A connection starts with one line saying what it wants ([`HELLO_MCP`] or
//! [`HELLO_SHOW`]); an MCP session follows the first.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use interprocess::local_socket::tokio::{prelude::*, Listener, Stream};
use interprocess::local_socket::{ListenerOptions, Name};
use tokio::io::AsyncWriteExt;

/// First line of a connection that carries an MCP session
pub const HELLO_MCP: &str = "radiotrope/1 mcp";
/// First line of a connection from a second launch: show the window
pub const HELLO_SHOW: &str = "radiotrope/1 show";

/// How long a second launch keeps trying to reach a player that is still
/// starting up
const SHOW_WAIT: Duration = Duration::from_secs(5);

/// The result of trying to become the player
pub enum Acquire {
    /// We are the player; keep the guard for as long as the process runs
    Primary(Instance),
    /// Another player holds the lock
    Running,
    /// The lock could not be taken for another reason; run on our own
    Unavailable(io::Error),
}

/// Held by the player for its whole life: dropping it releases the lock
pub struct Instance {
    dir: PathBuf,
    _lock: File,
}

impl Instance {
    /// Start listening for relays and second launches. Call inside a tokio
    /// runtime.
    pub fn listen(&self) -> io::Result<Listener> {
        listen_in(&self.dir)
    }
}

/// Try to become this user's player
pub fn acquire() -> Acquire {
    match runtime_dir() {
        Ok(dir) => acquire_in(dir),
        Err(e) => Acquire::Unavailable(e),
    }
}

fn acquire_in(dir: PathBuf) -> Acquire {
    // No truncate: on Windows another process's lock would make it fail
    let file = match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join("instance.lock"))
    {
        Ok(f) => f,
        Err(e) => return Acquire::Unavailable(e),
    };
    match file.try_lock() {
        Ok(()) => Acquire::Primary(Instance { dir, _lock: file }),
        Err(TryLockError::WouldBlock) => Acquire::Running,
        Err(TryLockError::Error(e)) => Acquire::Unavailable(e),
    }
}

/// Connect to the running player
pub async fn connect() -> io::Result<Stream> {
    connect_in(&runtime_dir()?).await
}

async fn connect_in(dir: &Path) -> io::Result<Stream> {
    let stream = Stream::connect(socket_name(dir)?).await?;
    // Another user could have made the pipe first, to read what agents
    // send: say nothing to one that isn't our own player's
    #[cfg(windows)]
    {
        let server = stream.peer_creds()?.pid();
        let theirs = server
            .ok_or_else(|| io::Error::other("no server process"))
            .and_then(windows_user::of_process)
            .map_err(|e| {
                io::Error::new(e.kind(), format!("can't tell who runs the player: {e}"))
            })?;
        if theirs != windows_user::ours()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the player's pipe belongs to another user",
            ));
        }
    }
    Ok(stream)
}

/// Ask the running player to show its window (from a second launch).
/// Retries for a few seconds while that player is still starting up.
pub fn ask_to_show() -> bool {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    runtime.block_on(async {
        let Ok(dir) = runtime_dir() else {
            return false;
        };
        let deadline = Instant::now() + SHOW_WAIT;
        loop {
            if let Ok(mut conn) = connect_in(&dir).await {
                return conn
                    .write_all(format!("{HELLO_SHOW}\n").as_bytes())
                    .await
                    .is_ok();
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
}

/// Start the player as its own process, detached from ours, so that it keeps
/// playing after the agent that started it has gone
pub fn spawn_player() -> io::Result<()> {
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(player_program()?);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Not the agent's working folder, which the player would keep busy (a
    // project on a USB stick could not be ejected while it plays)
    if let Some(home) = dirs::home_dir().filter(|dir| dir.is_dir()) {
        cmd.current_dir(home);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    for (var, value) in desktop_session_env() {
        cmd.env(var, value);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A session of its own: closing the agent's terminal or process
        // group doesn't take the player with it
        // SAFETY: setsid is async-signal-safe and touches no Rust state
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        // Without this the player would attach to the agent's terminal, and
        // closing that terminal would end it; it logs to a file instead
        cmd.env(crate::windows_console::NO_CONSOLE_ENV, "1");
        keep_std_handles_to_ourselves();
    }

    cmd.spawn().map(drop)
}

/// The program to start as the player: this one, or inside an AppImage the
/// AppImage file itself. The program running is in the AppImage's
/// temporary mount, which goes away when this relay exits.
fn player_program() -> io::Result<PathBuf> {
    match std::env::var_os("APPIMAGE").filter(|path| !path.is_empty()) {
        Some(appimage) => Ok(PathBuf::from(appimage)),
        None => std::env::current_exe(),
    }
}

/// The desktop session's variables that our environment lacks, for the
/// player we start. Agents may start `--mcp` with only HOME, PATH and a few
/// others (Claude Desktop does), and the player needs the display to open
/// its window on, and the runtime folder for sound and the session bus.
#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_session_env() -> Vec<(&'static str, std::ffi::OsString)> {
    use std::process::{Command, Stdio};

    let ours = |var: &str| std::env::var_os(var).filter(|v| !v.is_empty());
    if DISPLAY_VARS.iter().any(|v| ours(v).is_some()) && ours("XDG_RUNTIME_DIR").is_some() {
        return Vec::new();
    }
    let runtime = session_runtime_dir();
    // systemd keeps the session's variables; it needs the runtime folder to
    // be reached
    let mut systemctl = Command::new("systemctl");
    systemctl
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = &runtime {
        systemctl.env("XDG_RUNTIME_DIR", dir);
    }
    let listed = systemctl
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    let mut session: Vec<(String, String)> = parse_environment(&listed);
    // Without systemd's list: the usual places
    let has = |session: &[(String, String)], var: &str| session.iter().any(|(k, _)| k == var);
    if let Some(dir) = &runtime {
        if !has(&session, "XDG_RUNTIME_DIR") {
            session.push(("XDG_RUNTIME_DIR".into(), dir.display().to_string()));
        }
        if !DISPLAY_VARS.iter().any(|v| has(&session, v)) {
            if dir.join("wayland-0").exists() {
                session.push(("WAYLAND_DISPLAY".into(), "wayland-0".into()));
            } else if Path::new("/tmp/.X11-unix/X0").exists() {
                session.push(("DISPLAY".into(), ":0".into()));
            }
        }
    }
    missing_session_vars(ours, &session)
}

/// Any one of these says where to open a window
#[cfg(all(unix, not(target_os = "macos")))]
const DISPLAY_VARS: [&str; 3] = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY"];

/// The session variables worth passing on, and whether each belongs to the
/// display (those come as a set, or not at all)
#[cfg(all(unix, not(target_os = "macos")))]
const SESSION_VARS: [(&str, bool); 8] = [
    ("XDG_RUNTIME_DIR", false),
    ("DBUS_SESSION_BUS_ADDRESS", false),
    ("XDG_SESSION_TYPE", false),
    ("XDG_CURRENT_DESKTOP", false),
    ("WAYLAND_DISPLAY", true),
    ("WAYLAND_SOCKET", true),
    ("DISPLAY", true),
    ("XAUTHORITY", true),
];

/// `NAME=value` lines, as `systemctl --user show-environment` prints them
#[cfg(all(unix, not(target_os = "macos")))]
fn parse_environment(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// Which of the session's variables to add: those we lack, and the display
/// ones only when we have no display at all, so an agent that picked X11 or
/// Wayland keeps its pick
#[cfg(all(unix, not(target_os = "macos")))]
fn missing_session_vars(
    ours: impl Fn(&str) -> Option<std::ffi::OsString>,
    session: &[(String, String)],
) -> Vec<(&'static str, std::ffi::OsString)> {
    let have_display = DISPLAY_VARS.iter().any(|v| ours(v).is_some());
    SESSION_VARS
        .iter()
        .filter(|(var, display)| !(*display && have_display) && ours(var).is_none())
        .filter_map(|(var, _)| {
            let value = session.iter().find(|(k, _)| k == var)?.1.clone();
            (!value.is_empty()).then(|| (*var, value.into()))
        })
        .collect()
}

/// The pipes the agent gave us must not leak into the player: the agent
/// would then wait for the player to exit before it sees ours
#[cfg(windows)]
fn keep_std_handles_to_ourselves() {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
    }
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: plain Win32 calls on our own standard handles; a null or
        // invalid handle only makes SetHandleInformation fail
        unsafe {
            let handle = GetStdHandle(which);
            if !handle.is_null() && handle as isize != -1 {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

fn listen_in(dir: &Path) -> io::Result<Listener> {
    // Only the lock holder gets here, so a socket file left by a crashed
    // player is safe to remove
    #[cfg(unix)]
    match std::fs::remove_file(dir.join("mcp.sock")) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }

    let options = ListenerOptions::new().name(socket_name(dir)?);

    #[cfg(windows)]
    let options = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        // Full access for this user and the system, nobody else. The default
        // descriptor lets any local user open the pipe for reading. The user
        // by SID, not as the owner: an elevated player's pipe belongs to
        // Administrators, which would keep out the user's own relays.
        let sddl = format!("D:P(A;;GA;;;{})(A;;GA;;;SY)", windows_user::ours()?);
        let sddl = widestring::U16CString::from_str(sddl).map_err(io::Error::other)?;
        options.security_descriptor(SecurityDescriptor::deserialize(&sddl)?)
    };

    options.create_tokio()
}

#[cfg(unix)]
fn socket_name(dir: &Path) -> io::Result<Name<'static>> {
    use interprocess::local_socket::{GenericFilePath, ToFsName};
    Ok(dir
        .join("mcp.sock")
        .to_fs_name::<GenericFilePath>()?
        .into_owned())
}

#[cfg(windows)]
fn socket_name(dir: &Path) -> io::Result<Name<'static>> {
    use interprocess::local_socket::{GenericNamespaced, ToNsName};
    let name = pipe_name(&windows_user::ours()?, dir);
    Ok(name.to_ns_name::<GenericNamespaced>()?.into_owned())
}

/// The pipe of the user with SID `sid`. Named pipes are machine-wide: the
/// SID keeps two people on one computer apart, where names can share a
/// spelling once made ASCII, and needs nothing from the environment. Tests
/// pass their own folder, which names its own pipe.
#[cfg(any(windows, test))]
fn pipe_name(sid: &str, dir: &Path) -> String {
    match dir.file_name().and_then(|n| n.to_str()) {
        Some("radiotrope") | None => format!("radiotrope-{sid}"),
        Some(other) => format!("radiotrope-{sid}-{other}"),
    }
}

/// Who a process runs as: its user's SID, e.g. "S-1-5-21-...-1001"
#[cfg(windows)]
mod windows_user {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    type Handle = *mut c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    #[link(name = "advapi32")]
    extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: u32,
            info: *mut c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut c_void, text: *mut *mut u16) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const TOKEN_QUERY: u32 = 0x0008;
    /// `TokenUser` of TOKEN_INFORMATION_CLASS
    const TOKEN_USER: u32 = 1;

    /// This process's user
    pub fn ours() -> io::Result<String> {
        // SAFETY: no preconditions; the pseudo handle needs no closing
        token_user(unsafe { GetCurrentProcess() })
    }

    /// The user of process `pid`
    pub fn of_process(pid: u32) -> io::Result<String> {
        // SAFETY: plain Win32 call; null means it failed
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a handle we just opened, closed when this drops
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        token_user(process.as_raw_handle())
    }

    fn token_user(process: Handle) -> io::Result<String> {
        let mut token: Handle = std::ptr::null_mut();
        // SAFETY: `token` receives a handle on success
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the handle just received, closed when this drops
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        // TOKEN_USER: a pointer to the SID, its attributes, then the SID
        // itself (68 bytes at most); u64s keep the pointer aligned
        let mut info = [0u64; 16];
        let mut length = 0u32;
        // SAFETY: `info` is as long as said
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TOKEN_USER,
                info.as_mut_ptr().cast(),
                std::mem::size_of_val(&info) as u32,
                &mut length,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call filled in TOKEN_USER, which starts with the pointer
        let sid = unsafe { info.as_ptr().cast::<*mut c_void>().read() };
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` points into `info`, still alive; `text` receives a
        // string the system allocated
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a NUL-terminated string from the call above, freed once
        // copied
        let sid = unsafe { widestring::U16CStr::from_ptr_str(text) }.to_string_lossy();
        // SAFETY: `text` came from LocalAlloc and is not used again
        unsafe { LocalFree(text.cast()) };
        Ok(sid)
    }
}

/// The user's private folder for the lock and the socket
#[cfg(unix)]
fn runtime_dir() -> io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    // SAFETY: getuid has no preconditions and cannot fail
    let uid = unsafe { libc::getuid() };
    let dir = match session_runtime_dir() {
        Some(base) => base.join("radiotrope"),
        None => std::env::temp_dir().join(format!("radiotrope-{uid}")),
    };
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    // In a shared temp folder someone else could have made it first
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} belongs to another user", dir.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// `$XDG_RUNTIME_DIR`, or systemd's `/run/user/<uid>` when an agent started
/// us without it: the player (started from the desktop) uses that folder
#[cfg(unix)]
fn session_runtime_dir() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    // SAFETY: getuid has no preconditions and cannot fail
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/run/user/{uid}"));
    let meta = std::fs::metadata(&dir).ok()?;
    (meta.is_dir() && meta.uid() == uid).then_some(dir)
}

#[cfg(not(unix))]
fn runtime_dir() -> io::Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no local app data folder"))?
        .join("radiotrope");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-inst-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_trimmed_environment_gets_the_desktop_session() {
        let session = parse_environment(
            "LANG=en_US.UTF-8\nXDG_RUNTIME_DIR=/run/user/1000\nWAYLAND_DISPLAY=wayland-0\n\
             DISPLAY=:0\nXAUTHORITY=/run/user/1000/xauth\nDBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\n",
        );
        fn names(vars: Vec<(&'static str, std::ffi::OsString)>) -> Vec<&'static str> {
            vars.into_iter().map(|(k, _)| k).collect()
        }
        // Claude Desktop: HOME, PATH and the like only
        assert_eq!(
            names(missing_session_vars(|_| None, &session)),
            [
                "XDG_RUNTIME_DIR",
                "DBUS_SESSION_BUS_ADDRESS",
                "WAYLAND_DISPLAY",
                "DISPLAY",
                "XAUTHORITY"
            ]
        );
        // An agent on X11 keeps X11, and gets what else it lacks
        let on_x11 = |var: &str| (var == "DISPLAY").then(|| ":1".into());
        assert_eq!(
            names(missing_session_vars(on_x11, &session)),
            ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"]
        );
        // Nothing to add when nothing is known
        assert!(missing_session_vars(|_| None, &[]).is_empty());
    }

    #[test]
    fn each_user_has_a_pipe_of_their_own() {
        let sid = "S-1-5-21-1004336348-1177238915-682003330-1001";
        assert_eq!(
            pipe_name(sid, Path::new("C:/Users/José/AppData/Local/radiotrope")),
            format!("radiotrope-{sid}")
        );
        assert_ne!(
            pipe_name(sid, Path::new("radiotrope")),
            pipe_name(
                "S-1-5-21-1004336348-1177238915-682003330-1002",
                Path::new("radiotrope")
            )
        );
        // A test's own folder gets a pipe of its own
        assert_eq!(
            pipe_name(sid, Path::new("/tmp/rt-inst-sock-42")),
            format!("radiotrope-{sid}-rt-inst-sock-42")
        );
    }

    #[cfg(windows)]
    #[test]
    fn this_process_runs_as_its_user() {
        let ours = windows_user::ours().unwrap();
        assert!(ours.starts_with("S-1-"), "{ours}");
        assert_eq!(windows_user::of_process(std::process::id()).unwrap(), ours);
    }

    #[test]
    fn a_second_player_finds_the_lock_taken() {
        let dir = scratch_dir("lock");
        let first = acquire_in(dir.clone());
        assert!(matches!(first, Acquire::Primary(_)));
        assert!(matches!(acquire_in(dir.clone()), Acquire::Running));
        // The lock goes with the player
        drop(first);
        assert!(matches!(acquire_in(dir.clone()), Acquire::Primary(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_player_hears_what_a_client_writes() {
        let dir = scratch_dir("sock");
        let Acquire::Primary(instance) = acquire_in(dir.clone()) else {
            panic!("no lock");
        };
        let listener = instance.listen().unwrap();

        let mut client = connect_in(&dir).await.unwrap();
        client
            .write_all(format!("{HELLO_SHOW}\n").as_bytes())
            .await
            .unwrap();
        drop(client);

        let server = listener.accept().await.unwrap();
        let mut line = String::new();
        BufReader::new(server).read_line(&mut line).await.unwrap();
        assert_eq!(line.trim_end(), HELLO_SHOW);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_socket_left_by_a_crashed_player_is_replaced() {
        use tokio::io::AsyncReadExt;
        let dir = scratch_dir("stale");
        std::fs::write(dir.join("mcp.sock"), b"").unwrap();
        let Acquire::Primary(instance) = acquire_in(dir.clone()) else {
            panic!("no lock");
        };
        let listener = instance.listen().unwrap();
        let mut client = connect_in(&dir).await.unwrap();
        client.write_all(b"x").await.unwrap();
        let mut server = listener.accept().await.unwrap();
        let mut byte = [0u8];
        server.read_exact(&mut byte).await.unwrap();
        assert_eq!(&byte, b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_folder_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = runtime_dir().unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
