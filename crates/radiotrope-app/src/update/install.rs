//! Download a release, check its signature, and put it in place of the
//! running player, each only when the user asks.
//!
//! - **Windows setup install**: the new setup program runs with `/SILENT`
//!   (its progress window, no questions). It closes the player with
//!   `radiotrope --quit`, replaces the files and starts the new one
//!   (`/RELAUNCH`, see packaging/windows/radiotrope.iss).
//! - **AppImage**: the new file is written beside the old one and renamed
//!   over it, which the running player (it keeps the old file open) doesn't
//!   notice; the new one starts once this one has exited.
//!
//! The download is kept in memory until its signature is checked: a file
//! that isn't ours never reaches the disk as a program.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::verify::{self, VerifyError};
use super::{InstallKind, Mode, Version, RELEASES_URL};

/// Larger than any release file will be (they are 10 to 40 MB)
const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;
const MAX_SIGNATURE: u64 = 16 * 1024;

/// The release page of `version`: what is new, and the files
pub fn notes_url(version: &Version) -> String {
    format!("{RELEASES_URL}/tag/v{version}")
}

/// The release file that updates an install of this kind, e.g.
/// `Radiotrope-0.2.0-x86_64.AppImage`; `arch` as `std::env::consts::ARCH`
/// has it. `None` for installs that don't update themselves.
pub fn asset_name(install: &InstallKind, version: &Version, arch: &str) -> Option<String> {
    match install {
        // One setup program for x64 and ARM64
        InstallKind::WindowsSetup => Some(format!("radiotrope-{version}-setup.exe")),
        InstallKind::AppImage(_) => Some(format!("Radiotrope-{version}-{arch}.AppImage")),
        InstallKind::Other => None,
    }
}

/// Where a file of release `version` is downloaded from. The address is
/// made here, from the version alone: none is taken from a server's answer.
pub fn download_url(version: &Version, name: &str) -> String {
    format!("{RELEASES_URL}/download/v{version}/{name}")
}

#[derive(Debug, PartialEq, Eq)]
pub enum StageError {
    /// This build or this install doesn't update itself: nothing was sent
    Disabled,
    /// The download failed (offline, a timeout, no such file)
    Network(String),
    /// The download is not what we signed; nothing was kept
    Verify(VerifyError),
    /// It can't be saved
    Disk(String),
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StageError::Disabled => write!(f, "self-update is off"),
            StageError::Network(e) => write!(f, "download failed: {e}"),
            StageError::Verify(e) => write!(f, "not verified: {e}"),
            StageError::Disk(e) => write!(f, "can't save it: {e}"),
        }
    }
}

/// Where a release file and its signature come from (the network; the
/// tests' memory)
pub trait Source {
    /// The text of the `.minisig` file
    fn signature(&mut self) -> Result<String, StageError>;
    /// The file itself; `progress` is told the bytes so far and the total,
    /// when the server says it
    fn file(&mut self, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Vec<u8>, StageError>;
}

/// Download the release file `name`, check that it is signed with
/// `public_key`, and only then save it in `dir` (emptied first). Returns
/// where it is. Does nothing, and asks `source` for nothing, unless the
/// mode is [`Mode::SelfUpdate`].
pub fn stage(
    mode: Mode,
    name: &str,
    dir: &Path,
    source: &mut dyn Source,
    public_key: &str,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<PathBuf, StageError> {
    if mode != Mode::SelfUpdate {
        return Err(StageError::Disabled);
    }
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return Err(StageError::Disk("not a file name".into()));
    }
    // The signature first: without one there is no point in the download
    let signature = source.signature()?;
    let data = source.file(progress)?;
    verify::verify(&data, &signature, public_key, name).map_err(StageError::Verify)?;

    let disk = |e: io::Error| StageError::Disk(e.to_string());
    clear(dir);
    make_private_dir(dir).map_err(disk)?;
    let path = dir.join(name);
    let part = dir.join(format!("{name}.part"));
    std::fs::write(&part, &data).map_err(disk)?;
    make_executable(&part).map_err(disk)?;
    std::fs::rename(&part, &path).map_err(disk)?;
    Ok(path)
}

/// Download and check the update to `version` for this install, into the
/// cache folder
pub fn stage_release(
    mode: Mode,
    install: &InstallKind,
    version: &Version,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<PathBuf, StageError> {
    // Before anything is asked of the network
    if mode != Mode::SelfUpdate {
        return Err(StageError::Disabled);
    }
    let name = asset_name(install, version, std::env::consts::ARCH).ok_or(StageError::Disabled)?;
    let dir = staging_dir().ok_or_else(|| StageError::Disk("no cache folder".into()))?;
    let mut source = Http {
        url: download_url(version, &name),
    };
    stage(mode, &name, &dir, &mut source, verify::PUBLIC_KEY, progress)
}

/// Where downloads wait for the user's click: `updates` in the cache folder
pub fn staging_dir() -> Option<PathBuf> {
    crate::data::storage::cache_dir()
        .ok()
        .map(|dir| dir.join("updates"))
}

/// Remove what an earlier run downloaded (it was installed, or the user
/// never asked for it)
pub fn clear_staged() {
    if let Some(dir) = staging_dir() {
        clear(&dir);
    }
}

fn clear(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // Files only: nothing else is ever put here
        if entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The folder, for this user only
fn make_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn make_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// The release files on GitHub, over HTTPS
struct Http {
    url: String,
}

impl Source for Http {
    fn signature(&mut self) -> Result<String, StageError> {
        let network = |e: String| StageError::Network(e);
        let response = super::check::client(Some(std::time::Duration::from_secs(30)))
            .map_err(network)?
            .get(format!("{}.minisig", self.url))
            .send()
            .and_then(|response| response.error_for_status())
            .map_err(|e| network(e.to_string()))?;
        let mut text = String::new();
        response
            .take(MAX_SIGNATURE)
            .read_to_string(&mut text)
            .map_err(|e| network(e.to_string()))?;
        Ok(text)
    }

    fn file(&mut self, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Vec<u8>, StageError> {
        let network = |e: String| StageError::Network(e);
        // No time limit on the whole download: a slow line may take a while
        let mut response = super::check::client(None)
            .map_err(network)?
            .get(&self.url)
            .send()
            .and_then(|response| response.error_for_status())
            .map_err(|e| network(e.to_string()))?;
        let total = response.content_length().filter(|total| *total > 0);
        if total.is_some_and(|total| total > MAX_DOWNLOAD) {
            return Err(network("the file is too large".into()));
        }
        let mut data = Vec::with_capacity(total.unwrap_or(0) as usize);
        let mut chunk = vec![0u8; 64 * 1024];
        progress(0, total);
        loop {
            let read = response
                .read(&mut chunk)
                .map_err(|e| network(e.to_string()))?;
            if read == 0 {
                break;
            }
            data.extend_from_slice(&chunk[..read]);
            if data.len() as u64 > MAX_DOWNLOAD {
                return Err(network("the file is too large".into()));
            }
            progress(data.len() as u64, total);
        }
        if total.is_some_and(|total| total != data.len() as u64) {
            return Err(network("the download was cut short".into()));
        }
        Ok(data)
    }
}

/// What installing started
#[derive(Debug)]
pub enum Applied {
    /// The setup program is running. It closes the player itself; if it
    /// ends while the player still runs, nothing was installed.
    SetupStarted(Child),
    /// The AppImage file is the new version, and the new player starts
    /// when this one has exited: quit now
    Replaced,
}

/// The setup program's switches: its progress window and no questions, no
/// reboot, and start the player again when done
pub const SETUP_SWITCHES: [&str; 4] = ["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/RELAUNCH"];

/// Install the checked download at `staged` over this install
pub fn apply(install: &InstallKind, staged: &Path) -> io::Result<Applied> {
    match install {
        InstallKind::WindowsSetup => Command::new(staged)
            .args(SETUP_SWITCHES)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(Applied::SetupStarted),
        InstallKind::AppImage(target) => {
            replace_file(target, staged)?;
            start_when_we_exit(target)?;
            Ok(Applied::Replaced)
        }
        InstallKind::Other => Err(io::Error::other("this install doesn't update itself")),
    }
}

/// Put the file at `new` in place of `target`: copied into the same folder
/// under another name, then renamed over it, so `target` is at every moment
/// the whole old file or the whole new one. It keeps the old file's
/// permissions, and is executable.
pub fn replace_file(target: &Path, new: &Path) -> io::Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::other("no folder"))?;
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::other("no file name"))?;
    let mut hidden = std::ffi::OsString::from(".");
    hidden.push(name);
    hidden.push(".update");
    let temp = dir.join(hidden);

    let copied = (|| {
        std::fs::copy(new, &temp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(target).map_or(0o755, |meta| meta.permissions().mode());
            std::fs::set_permissions(
                &temp,
                std::fs::Permissions::from_mode((mode & 0o777) | 0o111),
            )?;
        }
        // Opened for writing: Windows flushes no file opened to read
        std::fs::OpenOptions::new()
            .write(true)
            .open(&temp)?
            .sync_all()?;
        std::fs::rename(&temp, target)
    })();
    if copied.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    copied
}

/// Start `program` once this process is gone: a second player started
/// earlier would only bring this one's window forward and exit. A small
/// shell waits for our process id to go away.
fn start_when_we_exit(program: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut waiter = Command::new("/bin/sh");
        waiter
            .arg("-c")
            .arg(r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; exec "$0""#)
            .arg(program)
            .arg(std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // What the AppImage runtime set for the old file is not passed on
        for var in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
            waiter.env_remove(var);
        }
        // A session of its own, so it outlives us however we were started
        // SAFETY: setsid is async-signal-safe and touches no Rust state
        unsafe {
            waiter.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        waiter.spawn().map(drop)
    }
    #[cfg(not(unix))]
    {
        let _ = program;
        Err(io::Error::other("not on this system"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::verify::tests::TestKey;
    use super::*;

    const NAME: &str = "Radiotrope-0.2.0-x86_64.AppImage";

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn temp_dir(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("radiotrope-update-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A release file in memory, counting what is asked of it
    struct Memory {
        signature: Result<String, StageError>,
        data: Vec<u8>,
        asked: u32,
    }

    impl Memory {
        fn new(data: &[u8], signature: String) -> Self {
            Self {
                signature: Ok(signature),
                data: data.to_vec(),
                asked: 0,
            }
        }
    }

    impl Source for Memory {
        fn signature(&mut self) -> Result<String, StageError> {
            self.asked += 1;
            std::mem::replace(&mut self.signature, Ok(String::new()))
        }

        fn file(
            &mut self,
            progress: &mut dyn FnMut(u64, Option<u64>),
        ) -> Result<Vec<u8>, StageError> {
            self.asked += 1;
            progress(self.data.len() as u64, Some(self.data.len() as u64));
            Ok(self.data.clone())
        }
    }

    #[test]
    fn each_install_has_its_release_file() {
        let version = v("0.2.0");
        assert_eq!(
            asset_name(&InstallKind::WindowsSetup, &version, "x86_64").as_deref(),
            Some("radiotrope-0.2.0-setup.exe")
        );
        assert_eq!(
            asset_name(&InstallKind::WindowsSetup, &version, "aarch64").as_deref(),
            Some("radiotrope-0.2.0-setup.exe")
        );
        let appimage = InstallKind::AppImage("/home/radio/Radiotrope.AppImage".into());
        assert_eq!(
            asset_name(&appimage, &version, "x86_64").as_deref(),
            Some(NAME)
        );
        assert_eq!(
            asset_name(&appimage, &version, "aarch64").as_deref(),
            Some("Radiotrope-0.2.0-aarch64.AppImage")
        );
        assert_eq!(asset_name(&InstallKind::Other, &version, "x86_64"), None);
    }

    #[test]
    fn the_addresses_are_the_projects_releases() {
        let version = v("0.2.0");
        assert_eq!(
            notes_url(&version),
            "https://github.com/goten002/radiotrope/releases/tag/v0.2.0"
        );
        assert_eq!(
            download_url(&version, NAME),
            "https://github.com/goten002/radiotrope/releases/download/v0.2.0/Radiotrope-0.2.0-x86_64.AppImage"
        );
    }

    #[test]
    fn a_signed_download_is_saved() {
        let dir = temp_dir("signed");
        let key = TestKey::new();
        let data = b"\x7fELF the new version";
        let mut source = Memory::new(data, key.sign(data, NAME));
        // What an earlier run left goes away
        std::fs::write(dir.join("Radiotrope-0.1.5-x86_64.AppImage"), b"old").unwrap();
        let mut seen = Vec::new();
        let path = stage(
            Mode::SelfUpdate,
            NAME,
            &dir,
            &mut source,
            &key.public(),
            &mut |done, total| seen.push((done, total)),
        )
        .expect("staged");
        assert_eq!(path, dir.join(NAME));
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(seen, [(data.len() as u64, Some(data.len() as u64))]);
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, [std::ffi::OsString::from(NAME)]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "executable");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_download_that_is_not_ours_is_never_saved() {
        let dir = temp_dir("unsigned");
        let ours = TestKey::new();
        let theirs = TestKey::new();
        let data = b"something else";
        let stage_with = |mut source: Memory| {
            stage(
                Mode::SelfUpdate,
                NAME,
                &dir,
                &mut source,
                &ours.public(),
                &mut |_, _| {},
            )
        };
        // Signed by someone else
        assert_eq!(
            stage_with(Memory::new(data, theirs.sign(data, NAME))),
            Err(StageError::Verify(VerifyError::Signature))
        );
        // Changed on the way
        assert_eq!(
            stage_with(Memory::new(b"something else!", ours.sign(data, NAME))),
            Err(StageError::Verify(VerifyError::Signature))
        );
        // Ours, but an older release
        assert_eq!(
            stage_with(Memory::new(
                data,
                ours.sign(data, "Radiotrope-0.1.0-x86_64.AppImage")
            )),
            Err(StageError::Verify(VerifyError::OtherFile))
        );
        // A web page instead of a signature
        assert_eq!(
            stage_with(Memory::new(data, "<html>Not Found</html>".into())),
            Err(StageError::Verify(VerifyError::Format))
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "nothing reaches the disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_signature_nothing_is_downloaded() {
        let dir = temp_dir("nosig");
        let key = TestKey::new();
        let mut source = Memory::new(b"data", String::new());
        source.signature = Err(StageError::Network("404".into()));
        let staged = stage(
            Mode::SelfUpdate,
            NAME,
            &dir,
            &mut source,
            &key.public(),
            &mut |_, _| {},
        );
        assert_eq!(staged, Err(StageError::Network("404".into())));
        assert_eq!(source.asked, 1, "the file itself was not asked for");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_build_that_does_not_update_itself_downloads_nothing() {
        let dir = temp_dir("gate");
        let key = TestKey::new();
        let data = b"the new version";
        for mode in [Mode::Off, Mode::Notify] {
            let mut source = Memory::new(data, key.sign(data, NAME));
            let staged = stage(mode, NAME, &dir, &mut source, &key.public(), &mut |_, _| {});
            assert_eq!(staged, Err(StageError::Disabled), "{mode:?}");
            assert_eq!(source.asked, 0, "{mode:?} asked the network");
            // The real one stops before it makes a request, too
            assert_eq!(
                stage_release(
                    mode,
                    &InstallKind::WindowsSetup,
                    &v("0.2.0"),
                    &mut |_, _| {}
                ),
                Err(StageError::Disabled)
            );
        }
        // An install that can't be replaced, whatever the mode
        assert_eq!(
            stage_release(
                Mode::SelfUpdate,
                &InstallKind::Other,
                &v("0.2.0"),
                &mut |_, _| {}
            ),
            Err(StageError::Disabled)
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_with_a_folder_in_it_is_refused() {
        let dir = temp_dir("names");
        let key = TestKey::new();
        for name in ["../evil", "a/b", r"a\b", "", ".hidden"] {
            let mut source = Memory::new(b"data", key.sign(b"data", name));
            let staged = stage(
                Mode::SelfUpdate,
                name,
                &dir,
                &mut source,
                &key.public(),
                &mut |_, _| {},
            );
            assert!(matches!(staged, Err(StageError::Disk(_))), "{name:?}");
            assert_eq!(source.asked, 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_new_file_takes_the_old_ones_place() {
        let dir = temp_dir("replace");
        let target = dir.join("Radiotrope.AppImage");
        let new = dir.join("staged");
        std::fs::write(&target, b"old version").unwrap();
        std::fs::write(&new, b"new version, longer").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o750)).unwrap();
        }
        replace_file(&target, &new).expect("replaced");
        assert_eq!(std::fs::read(&target).unwrap(), b"new version, longer");
        assert!(new.exists(), "the download is left for the cache to clear");
        assert!(!dir.join(".Radiotrope.AppImage.update").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o751, "the old permissions, executable");
        }
        // A download that has gone leaves the old file as it was
        std::fs::remove_file(&new).unwrap();
        assert!(replace_file(&target, &new).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"new version, longer");
        assert!(!dir.join(".Radiotrope.AppImage.update").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_setup_program_asks_nothing_and_starts_the_player_again() {
        assert!(SETUP_SWITCHES.contains(&"/SILENT"));
        assert!(SETUP_SWITCHES.contains(&"/RELAUNCH"));
        // The switch the setup script looks for (packaging/windows/radiotrope.iss)
        let script = include_str!("../../../../packaging/windows/radiotrope.iss");
        assert!(script.contains("HasParam('/RELAUNCH')"));
        assert!(apply(&InstallKind::Other, Path::new("nothing")).is_err());
    }
}
