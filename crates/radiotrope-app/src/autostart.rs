//! View > System Tray > Start at Login
//!
//! - Linux: the desktop's autostart file,
//!   `$XDG_CONFIG_HOME/autostart/radiotrope.desktop`
//! - Windows: the `Radiotrope` value of
//!   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! - macOS: a LaunchAgent,
//!   `~/Library/LaunchAgents/app.radiotrope.Radiotrope.plist`, which
//!   launchd reads when the user logs in
//!
//! Whether it is on is read from there, not from the settings, so removing
//! the file or the value by hand (or in the desktop's own startup settings)
//! shows in the menu.
//!
//! macOS 13 and later also list the player in System Settings > General >
//! Login Items ("Allow in the Background"). Turned off there, the file
//! stays and the menu's tick with it: macOS keeps that switch to itself.

use std::io;

/// Whether the player starts when the user logs in
pub fn is_enabled() -> bool {
    imp::is_enabled()
}

/// Start the player at login, or stop doing so
pub fn set_enabled(on: bool) -> io::Result<()> {
    imp::set_enabled(on)
}

/// The program to start: the AppImage itself when run from one (the
/// program inside lives on a mount that is gone after logout), the snap's
/// command in a snap, else this program.
///
/// In a snap the file lands in the snap's own `~/.config/autostart`
/// (`$SNAP_USER_DATA/.config`), which snapd's session helper reads because
/// the app declares `autostart: radiotrope.desktop`; it starts the app
/// itself and keeps only the arguments of `Exec`.
#[cfg_attr(
    not(any(target_os = "linux", windows, target_os = "macos")),
    allow(dead_code)
)]
fn program() -> io::Result<std::path::PathBuf> {
    if cfg!(target_os = "linux") {
        if let Some(snap) = radiotrope_app::snap::current() {
            return Ok(snap.command);
        }
        if let Some(appimage) = std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()) {
            return Ok(appimage.into());
        }
    }
    std::env::current_exe()
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io;
    use std::path::{Path, PathBuf};

    /// `~/.config/autostart/radiotrope.desktop`
    fn path() -> io::Result<PathBuf> {
        dirs::config_dir()
            .map(|dir| dir.join("autostart").join("radiotrope.desktop"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config folder (HOME unset)"))
    }

    pub fn is_enabled() -> bool {
        path().is_ok_and(|path| is_enabled_at(&path))
    }

    /// The file is there and not turned off in it (`Hidden=true`, or the
    /// GNOME key some startup settings write)
    pub(super) fn is_enabled_at(path: &Path) -> bool {
        std::fs::read_to_string(path).is_ok_and(|text| {
            !text
                .lines()
                .map(str::trim)
                .any(|line| line == "Hidden=true" || line == "X-GNOME-Autostart-enabled=false")
        })
    }

    pub fn set_enabled(on: bool) -> io::Result<()> {
        let path = path()?;
        if on {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let program = super::program()?;
            std::fs::write(&path, desktop_entry(&program.to_string_lossy()))
        } else {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        }
    }

    /// The autostart file that starts `program`
    pub(super) fn desktop_entry(program: &str) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Radiotrope\n\
             Comment=Internet radio player\n\
             Exec={}\n\
             Icon={}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            exec_arg(program),
            radiotrope_app::config::APP_ID,
        )
    }

    /// A program path as the `Exec` key wants it: `%` doubled, quoted when
    /// it holds a space or a character the key reserves, and then the
    /// key's own backslash escape on top
    pub(super) fn exec_arg(program: &str) -> String {
        const RESERVED: &str = " \t\n\"'\\><~|&;$*?#()`";
        let mut arg = program.replace('%', "%%");
        if arg.chars().any(|c| RESERVED.contains(c)) {
            let mut quoted = String::from("\"");
            for c in arg.chars() {
                if matches!(c, '"' | '`' | '$' | '\\') {
                    quoted.push('\\');
                }
                quoted.push(c);
            }
            quoted.push('"');
            arg = quoted;
        }
        arg.replace('\\', "\\\\")
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io;

    const HKEY_CURRENT_USER: isize = 0x8000_0001_u32 as i32 as isize;
    const REG_SZ: u32 = 1;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    const ERROR_SUCCESS: i32 = 0;
    const ERROR_FILE_NOT_FOUND: i32 = 2;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "Radiotrope";

    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            key: isize,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut c_void,
            size: *mut u32,
        ) -> i32;
        fn RegSetKeyValueW(
            key: isize,
            sub_key: *const u16,
            value: *const u16,
            kind: u32,
            data: *const c_void,
            size: u32,
        ) -> i32;
        fn RegDeleteKeyValueW(key: isize, sub_key: *const u16, value: *const u16) -> i32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn is_enabled() -> bool {
        let (key, value) = (wide(RUN_KEY), wide(VALUE));
        // SAFETY: null data asks only whether the value is there (and its size)
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        status == ERROR_SUCCESS
    }

    pub fn set_enabled(on: bool) -> io::Result<()> {
        let (key, value) = (wide(RUN_KEY), wide(VALUE));
        let status = if on {
            let program = super::program()?;
            let command = wide(&format!("\"{}\"", program.display()));
            // SAFETY: valid, NUL-terminated wide strings; the size in bytes
            // includes the terminator, as REG_SZ wants
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    value.as_ptr(),
                    REG_SZ,
                    command.as_ptr().cast(),
                    (command.len() * 2) as u32,
                )
            }
        } else {
            // SAFETY: valid, NUL-terminated wide strings
            match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) } {
                ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
                status => status,
            }
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status))
        }
    }
}

/// The file moved with the program: if Start at Login is on for another
/// copy of the player (the app was moved to Applications since), it is
/// written again for this one. macOS only; nothing to do elsewhere, where
/// packages keep the program in one place.
pub fn keep_current() {
    #[cfg(target_os = "macos")]
    imp::keep_current();
}

/// The LaunchAgent file's text and state. Compiled for the tests on every
/// system: only macOS writes the file.
#[cfg(any(target_os = "macos", test))]
mod launch_agent {
    use std::io;
    use std::path::{Path, PathBuf};

    /// `<home>/Library/LaunchAgents/app.radiotrope.Radiotrope.plist`
    pub fn path_in(home: &Path) -> PathBuf {
        home.join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", radiotrope_app::config::APP_ID))
    }

    /// The LaunchAgent that starts `program` when the user logs in:
    /// - `RunAtLoad`: once, when launchd reads the file at login; not kept
    ///   alive, so Quit stays quit;
    /// - `LimitLoadToSessionType` `Aqua`: only in a session at the screen
    ///   (not over SSH), where a window can open;
    /// - `ProcessType` `Interactive`: an app the user works with, not a
    ///   background job to slow down;
    /// - `AssociatedBundleIdentifiers`: macOS 13+ lists it in Login Items
    ///   under the app's name and icon.
    pub fn plist(program: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>Label</key>\n\
             \t<string>{id}</string>\n\
             \t<key>ProgramArguments</key>\n\
             \t<array>\n\
             \t\t<string>{program}</string>\n\
             \t</array>\n\
             \t<key>RunAtLoad</key>\n\
             \t<true/>\n\
             \t<key>LimitLoadToSessionType</key>\n\
             \t<string>Aqua</string>\n\
             \t<key>ProcessType</key>\n\
             \t<string>Interactive</string>\n\
             \t<key>AssociatedBundleIdentifiers</key>\n\
             \t<string>{id}</string>\n\
             </dict>\n\
             </plist>\n",
            id = radiotrope_app::config::APP_ID,
            program = xml_text(program),
        )
    }

    /// Text as XML character data
    fn xml_text(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    /// The file is there and not turned off in it (`Disabled`, which
    /// `launchctl unload -w` used to write)
    pub fn is_enabled_at(path: &Path) -> bool {
        std::fs::read(path)
            .is_ok_and(|bytes| bool_key(&String::from_utf8_lossy(&bytes), "Disabled") != Some(true))
    }

    /// `<key>name</key>` followed by `<true/>` or `<false/>`
    fn bool_key(plist: &str, name: &str) -> Option<bool> {
        let (_, value) = plist.split_once(&format!("<key>{name}</key>"))?;
        let value = value.trim_start();
        if value.starts_with("<true/>") {
            Some(true)
        } else if value.starts_with("<false/>") {
            Some(false)
        } else {
            None
        }
    }

    /// Write the file for `program`, or remove it
    pub fn set_enabled_at(path: &Path, on: bool, program: &Path) -> io::Result<()> {
        if on {
            let program = usable_program(program)?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, plist(program))
        } else {
            match std::fs::remove_file(path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        }
    }

    /// A program launchd can start at the next login. Not a copy run
    /// straight from the disk image or the Downloads folder: macOS runs
    /// that from a path made for the run ("translocation"), gone later.
    fn usable_program(program: &Path) -> io::Result<&str> {
        let text = program.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the program's path is not text",
            )
        })?;
        if text.contains("/AppTranslocation/") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "move Radiotrope to the Applications folder first, then open it from there",
            ));
        }
        Ok(text)
    }

    /// The file is on but starts another program than `program` (the app
    /// was moved): write it again for this one. Leaves a file that is off,
    /// or already right, alone.
    pub fn keep_current_at(path: &Path, program: &Path) -> io::Result<bool> {
        let Some(text) = program.to_str() else {
            return Ok(false);
        };
        let stale = is_enabled_at(path)
            && std::fs::read_to_string(path).is_ok_and(|written| written != plist(text));
        if !stale {
            return Ok(false);
        }
        set_enabled_at(path, true, program).map(|()| true)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::launch_agent;
    use std::io;
    use std::path::PathBuf;

    /// `~/Library/LaunchAgents/app.radiotrope.Radiotrope.plist`
    pub(super) fn path() -> io::Result<PathBuf> {
        dirs::home_dir()
            .map(|home| launch_agent::path_in(&home))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home folder (HOME unset)"))
    }

    pub fn is_enabled() -> bool {
        path().is_ok_and(|path| launch_agent::is_enabled_at(&path))
    }

    pub fn set_enabled(on: bool) -> io::Result<()> {
        // Removing the file needs no program
        let program = if on {
            super::program()?
        } else {
            PathBuf::new()
        };
        launch_agent::set_enabled_at(&path()?, on, &program)
    }

    pub fn keep_current() {
        let (Ok(path), Ok(program)) = (path(), super::program()) else {
            return;
        };
        match launch_agent::keep_current_at(&path, &program) {
            Ok(true) => eprintln!("Start at Login now starts {}", program.display()),
            Ok(false) => {}
            // A copy that can't be started at login keeps the old file
            Err(e) => eprintln!("Start at Login still starts another copy: {e}"),
        }
    }
}

/// Where the LaunchAgent file is, for `--start-at-login` to say
#[cfg(target_os = "macos")]
pub fn launch_agent_path() -> io::Result<std::path::PathBuf> {
    imp::path()
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod imp {
    use std::io;

    pub fn is_enabled() -> bool {
        false
    }

    pub fn set_enabled(_on: bool) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "not available on this system",
        ))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::imp::*;

    #[test]
    fn the_autostart_file_starts_this_program() {
        let entry = desktop_entry("/usr/bin/radiotrope");
        assert_eq!(
            entry,
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Radiotrope\n\
             Comment=Internet radio player\n\
             Exec=/usr/bin/radiotrope\n\
             Icon=app.radiotrope.Radiotrope\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n"
        );
    }

    #[test]
    fn a_path_with_spaces_or_reserved_characters_is_quoted() {
        assert_eq!(
            exec_arg("/home/a b/Radiotrope.AppImage"),
            "\"/home/a b/Radiotrope.AppImage\""
        );
        assert_eq!(exec_arg("/opt/100%/radiotrope"), "/opt/100%%/radiotrope");
        // `$` is escaped inside the quotes, and that backslash doubled
        // for the key itself
        assert_eq!(exec_arg("/opt/$x/radiotrope"), "\"/opt/\\\\$x/radiotrope\"");
    }

    #[test]
    fn a_hidden_entry_is_off() {
        let dir = std::env::temp_dir().join(format!("rt-autostart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("radiotrope.desktop");

        assert!(!is_enabled_at(&path), "no file");
        std::fs::write(&path, desktop_entry("/usr/bin/radiotrope")).unwrap();
        assert!(is_enabled_at(&path));
        std::fs::write(
            &path,
            desktop_entry("/usr/bin/radiotrope") + "Hidden=true\n",
        )
        .unwrap();
        assert!(!is_enabled_at(&path));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod launch_agent_tests {
    use super::launch_agent::*;
    use std::path::{Path, PathBuf};

    const PROGRAM: &str = "/Applications/Radiotrope.app/Contents/MacOS/radiotrope";

    /// A home folder of the test's own
    fn temp_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("rt-login-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn the_launch_agent_starts_this_program_at_login() {
        assert_eq!(
            plist(PROGRAM),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>Label</key>\n\
             \t<string>app.radiotrope.Radiotrope</string>\n\
             \t<key>ProgramArguments</key>\n\
             \t<array>\n\
             \t\t<string>/Applications/Radiotrope.app/Contents/MacOS/radiotrope</string>\n\
             \t</array>\n\
             \t<key>RunAtLoad</key>\n\
             \t<true/>\n\
             \t<key>LimitLoadToSessionType</key>\n\
             \t<string>Aqua</string>\n\
             \t<key>ProcessType</key>\n\
             \t<string>Interactive</string>\n\
             \t<key>AssociatedBundleIdentifiers</key>\n\
             \t<string>app.radiotrope.Radiotrope</string>\n\
             </dict>\n\
             </plist>\n"
        );
    }

    #[test]
    fn a_path_is_written_as_xml_text() {
        let text = plist("/Users/a&b/<Apps>/Radiotrope.app/Contents/MacOS/radiotrope");
        assert!(
            text.contains(
                "<string>/Users/a&amp;b/&lt;Apps&gt;/Radiotrope.app/Contents/MacOS/radiotrope</string>"
            ),
            "{text}"
        );
        // Spaces and other letters need nothing
        let text = plist("/Users/Άννα/My Apps/Radiotrope.app/Contents/MacOS/radiotrope");
        assert!(
            text.contains(
                "<string>/Users/Άννα/My Apps/Radiotrope.app/Contents/MacOS/radiotrope</string>"
            ),
            "{text}"
        );
    }

    #[test]
    fn the_file_is_in_the_users_launch_agents() {
        assert_eq!(
            path_in(Path::new("/Users/anna")),
            Path::new("/Users/anna/Library/LaunchAgents/app.radiotrope.Radiotrope.plist")
        );
    }

    #[test]
    fn turning_it_on_writes_the_file_and_off_removes_it() {
        let home = temp_home("on-off");
        let path = path_in(&home);
        assert!(!is_enabled_at(&path), "no file");
        // Off with no file: nothing to do, and no error
        set_enabled_at(&path, false, Path::new("")).unwrap();

        // `Library/LaunchAgents` is made when missing (a new account)
        set_enabled_at(&path, true, Path::new(PROGRAM)).unwrap();
        assert!(is_enabled_at(&path));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), plist(PROGRAM));

        // macOS's own tool reads it as a property list
        #[cfg(target_os = "macos")]
        {
            let lint = std::process::Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(&path)
                .output()
                .unwrap();
            assert!(lint.status.success(), "{lint:?}");
            let program = std::process::Command::new("/usr/bin/plutil")
                .args(["-extract", "ProgramArguments.0", "raw"])
                .arg(&path)
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&program.stdout).trim(), PROGRAM);
        }

        set_enabled_at(&path, false, Path::new("")).unwrap();
        assert!(!path.exists());
        assert!(!is_enabled_at(&path));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_file_turned_off_by_hand_is_off() {
        let home = temp_home("disabled");
        let path = path_in(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let disabled = |value: &str| {
            plist(PROGRAM).replace(
                "</dict>",
                &format!("\t<key>Disabled</key>\n\t{value}\n</dict>"),
            )
        };
        std::fs::write(&path, disabled("<true/>")).unwrap();
        assert!(!is_enabled_at(&path));
        std::fs::write(&path, disabled("<false/>")).unwrap();
        assert!(is_enabled_at(&path));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_copy_run_from_the_disk_image_is_not_started_at_login() {
        let home = temp_home("translocated");
        let path = path_in(&home);
        let translocated = Path::new(
            "/private/var/folders/ab/T/AppTranslocation/1234/d/Radiotrope.app/Contents/MacOS/radiotrope",
        );
        let error = set_enabled_at(&path, true, translocated).unwrap_err();
        assert!(error.to_string().contains("Applications"), "{error}");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_moved_app_is_started_from_where_it_is_now() {
        let home = temp_home("moved");
        let path = path_in(&home);
        let old = Path::new("/Users/anna/Downloads/Radiotrope.app/Contents/MacOS/radiotrope");
        let now = Path::new(PROGRAM);

        // Off: stays off
        assert!(!keep_current_at(&path, now).unwrap());
        assert!(!path.exists());

        set_enabled_at(&path, true, old).unwrap();
        assert!(keep_current_at(&path, now).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), plist(PROGRAM));
        // Right already
        assert!(!keep_current_at(&path, now).unwrap());
        let _ = std::fs::remove_dir_all(&home);
    }
}
