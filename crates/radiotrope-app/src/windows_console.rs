//! Where the Windows release build's output goes
//!
//! The release build is a GUI program (no console window of its own), so
//! `eprintln!` and panic messages would go nowhere. Started from a terminal,
//! it writes there; started by an agent, its pipes are kept; otherwise
//! (Explorer, the Start menu, a player an agent started) the output goes to
//! a log file in the local app data folder.

use std::ffi::c_void;
use std::path::PathBuf;

/// Set on the player [`crate::instance::spawn_player`] starts: it must not
/// share the agent's terminal, or closing that terminal would end it
pub const NO_CONSOLE_ENV: &str = "RADIOTROPE_NO_CONSOLE";

/// The log is started afresh (the old one kept as `.old.log`) past this size
const LOG_MAX_BYTES: u64 = 1024 * 1024;

const STD_ERROR_HANDLE: u32 = -12i32 as u32;
const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
const FILE_TYPE_DISK: u32 = 0x0001;
const FILE_TYPE_PIPE: u32 = 0x0003;

#[link(name = "kernel32")]
extern "system" {
    fn AttachConsole(process_id: u32) -> i32;
    fn GetStdHandle(std_handle: u32) -> *mut c_void;
    fn SetStdHandle(std_handle: u32, handle: *mut c_void) -> i32;
    fn GetFileType(file: *mut c_void) -> u32;
    fn GetConsoleMode(console: *mut c_void, mode: *mut u32) -> i32;
    fn SetConsoleCtrlHandler(handler: *const c_void, add: i32) -> i32;
}

/// Pick where this process writes: run before anything prints
pub fn set_up() {
    if std::env::var_os(NO_CONSOLE_ENV).is_none() {
        attach_parent_console();
    }
    if !stderr_is_usable() {
        log_to_file();
    }
}

/// When started from a terminal, write there: `--help`, `--version` and the
/// logs. A GUI program (the Windows release build) gets no console of its own.
///
/// Handles the parent passed in, such as the pipes an MCP client gives
/// `radiotrope --mcp`, are kept: Windows only replaces the standard handles
/// when the process was started without them.
fn attach_parent_console() {
    // SAFETY: plain Win32 calls with constant arguments
    unsafe {
        // Fails when there is no parent console (started from Explorer) or
        // the program already has one (a debug build), and both are fine
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            // The terminal has moved on to its next prompt: a Ctrl+C typed
            // there later is meant for something else, not for the player
            SetConsoleCtrlHandler(std::ptr::null(), 1);
        }
    }
}

/// Whether stderr reaches someone: a console, a pipe (an agent, a parent
/// process) or a file the user redirected it to. Not when it is missing or
/// the NUL device.
fn stderr_is_usable() -> bool {
    // SAFETY: plain Win32 queries on our own standard handle; a null or
    // invalid handle only makes them fail
    unsafe {
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        if handle.is_null() || handle as isize == -1 {
            return false;
        }
        let mut mode = 0;
        GetConsoleMode(handle, &mut mode) != 0
            || matches!(GetFileType(handle), FILE_TYPE_DISK | FILE_TYPE_PIPE)
    }
}

/// `%LOCALAPPDATA%\radiotrope\radiotrope.log`
pub fn log_path() -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join(radiotrope_app::config::app::NAME)
            .join("radiotrope.log"),
    )
}

/// Send stderr (`eprintln!`, panic messages) to [`log_path`]
fn log_to_file() {
    use std::io::Write;
    use std::os::windows::io::IntoRawHandle;

    let Some(path) = log_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("old.log"));
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = writeln!(
        file,
        "--- radiotrope {} started {} (pid {})",
        env!("CARGO_PKG_VERSION"),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        std::process::id()
    );
    // The handle stays open for the life of the process. Rust's stderr looks
    // the standard handle up on every write, so this takes effect at once.
    // SAFETY: a valid handle we own and never close
    unsafe {
        SetStdHandle(STD_ERROR_HANDLE, file.into_raw_handle());
    }
}
