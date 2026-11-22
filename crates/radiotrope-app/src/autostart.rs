//! View > System Tray > Start at Login
//!
//! - Linux: the desktop's autostart file,
//!   `$XDG_CONFIG_HOME/autostart/radiotrope.desktop`
//! - Windows: the `Radiotrope` value of
//!   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//!
//! Whether it is on is read from there, not from the settings, so removing
//! the file or the value by hand (or in the desktop's own startup settings)
//! shows in the menu.

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
/// program inside lives on a mount that is gone after logout), else this
/// program
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn program() -> io::Result<std::path::PathBuf> {
    if cfg!(target_os = "linux") {
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

#[cfg(not(any(target_os = "linux", windows)))]
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
