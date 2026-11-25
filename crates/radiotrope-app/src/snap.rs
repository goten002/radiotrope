//! Running as a snap (strict confinement)
//!
//! snapd starts the programs with `SNAP` (where the snap is mounted),
//! `SNAP_INSTANCE_NAME` (the snap's name, with `_<key>` for a parallel
//! install) and `SNAP_REAL_HOME` (the user's home; `HOME` is the snap's own
//! folder, `~/snap/radiotrope/<revision>`). Nothing here is compiled in: the
//! same program behaves as before outside a snap.

use std::ffi::OsString;
use std::path::PathBuf;

/// The snap's facts, from the environment
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snap {
    /// `/snap/bin/radiotrope`: the command that starts the player from
    /// outside the snap (an agent's `--mcp`, the autostart file). The
    /// program's own path is inside one revision of the snap and changes
    /// with every update.
    pub command: PathBuf,
    /// The user's real home, where recordings go by default
    pub real_home: Option<PathBuf>,
}

/// The snap this program runs in, if any
pub fn current() -> Option<Snap> {
    from_env(|name| std::env::var_os(name))
}

/// [`current`] with the environment given, for tests
pub fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Option<Snap> {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    set("SNAP")?;
    let name = set("SNAP_INSTANCE_NAME").or_else(|| set("SNAP_NAME"))?;
    // The main app is named after the snap, so its command is the snap's
    // name in /snap/bin
    let command = PathBuf::from("/snap/bin").join(name);
    let real_home = set("SNAP_REAL_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    Some(Snap { command, real_home })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn not_a_snap_without_snap() {
        assert_eq!(from_env(env(&[("SNAP_NAME", "radiotrope")])), None);
        assert_eq!(
            from_env(env(&[("SNAP", ""), ("SNAP_NAME", "radiotrope")])),
            None
        );
    }

    #[test]
    fn the_command_is_in_snap_bin() {
        let snap = from_env(env(&[
            ("SNAP", "/snap/radiotrope/12"),
            ("SNAP_NAME", "radiotrope"),
            ("SNAP_INSTANCE_NAME", "radiotrope"),
            ("SNAP_REAL_HOME", "/home/radio"),
        ]))
        .unwrap();
        assert_eq!(snap.command, PathBuf::from("/snap/bin/radiotrope"));
        assert_eq!(snap.real_home, Some(PathBuf::from("/home/radio")));
    }

    #[test]
    fn a_parallel_install_has_its_own_command() {
        let snap = from_env(env(&[
            ("SNAP", "/snap/radiotrope_beta/3"),
            ("SNAP_NAME", "radiotrope"),
            ("SNAP_INSTANCE_NAME", "radiotrope_beta"),
        ]))
        .unwrap();
        assert_eq!(snap.command, PathBuf::from("/snap/bin/radiotrope_beta"));
        assert_eq!(snap.real_home, None);
    }
}
