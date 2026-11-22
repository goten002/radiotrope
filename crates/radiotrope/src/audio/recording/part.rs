//! Recordings in progress
//!
//! A recording is written under a temporary name in its folder,
//! `.<name>.part` (hidden on Linux and macOS by the dot, and on Windows by
//! the hidden attribute), and gets its own name once the file is complete.
//! File managers then don't make thumbnails of, or index, half-written
//! files, and nobody opens an incomplete one.
//!
//! A recording cut short by a crash leaves its `.part` behind;
//! [`recover_partial_recordings`] gives it its name (what was written plays)
//! or removes it when it holds no audio.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::audio::decoder::SymphoniaSource;

/// Ending of the temporary name
const PART_SUFFIX: &str = ".part";

/// Extensions of the recordings we write (see `RecordingFormat::extension`)
const OUR_EXTENSIONS: &[&str] = &["mp3", "opus", "wav"];

/// The temporary name `path` is written under: `dir/.name.part`
pub fn part_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}{PART_SUFFIX}"))
}

/// The name a temporary file of ours gets when it is complete, or `None`
/// when `part_name` is not one of ours (`.<name>.<mp3|opus|wav>.part`)
fn final_name(part_name: &str) -> Option<&str> {
    let name = part_name.strip_prefix('.')?.strip_suffix(PART_SUFFIX)?;
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && OUR_EXTENSIONS.contains(&ext)).then_some(name)
}

/// `path`, or when that is taken `stem (2).ext`, `stem (3).ext`...
fn free_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("some numbered name is free")
}

/// Create the temporary file for a recording to `path`: it must not exist
/// yet. It is locked while it is written, so that recovery (in this or
/// another Radiotrope) leaves it alone.
pub(super) fn create(path: &Path) -> io::Result<File> {
    if path.exists() {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.attributes(windows::FILE_ATTRIBUTE_HIDDEN);
    }
    let file = options.open(part_path(path))?;
    // A file system without locks (some network shares) still records
    let _ = file.try_lock();
    Ok(file)
}

/// Give the complete (and closed) temporary file of `wanted` its name: that
/// one, or a numbered one when it was taken meanwhile. Returns where it is.
pub(super) fn finish(wanted: &Path) -> io::Result<PathBuf> {
    publish(&part_path(wanted), wanted)
}

fn publish(part: &Path, wanted: &Path) -> io::Result<PathBuf> {
    let target = free_path(wanted);
    fs::rename(part, &target)?;
    #[cfg(windows)]
    windows::unhide(&target);
    Ok(target)
}

/// What [`recover_partial_recordings`] did with a leftover file
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovered {
    /// It had audio and now has its name
    Saved(PathBuf),
    /// It had no audio and was removed
    Removed(PathBuf),
}

/// Tidy up recordings in `dir` that a crash or a power cut left as
/// temporary files: one with audio gets its name (it plays up to where it
/// got), one without is removed. Only our own names are looked at, and a
/// recording still being written (locked) is left alone. Each file dealt
/// with is logged.
pub fn recover_partial_recordings(dir: &Path) -> Vec<Recovered> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut done = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(final_name) = name.to_str().and_then(final_name) else {
            continue;
        };
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let part = entry.path();
        let wanted = dir.join(final_name);
        match recover(&part, &wanted) {
            Ok(Some(outcome)) => {
                match &outcome {
                    Recovered::Saved(path) => {
                        log::info!("Recovered an unfinished recording: {}", path.display())
                    }
                    Recovered::Removed(path) => log::info!(
                        "Removed an unfinished recording with no audio: {}",
                        path.display()
                    ),
                }
                done.push(outcome);
            }
            Ok(None) => {}
            Err(e) => log::warn!(
                "Can't recover the unfinished recording {}: {e}",
                part.display()
            ),
        }
    }
    done
}

/// Deal with one leftover; `None` when it is still being written
fn recover(part: &Path, wanted: &Path) -> io::Result<Option<Recovered>> {
    let file = File::open(part)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(None),
        // No locks on this file system: go by the name alone
        Err(fs::TryLockError::Error(_)) => {}
    }
    let ext = wanted.extension().and_then(|e| e.to_str());
    let audio = has_audio(file.try_clone()?, ext);
    // Closed before it is renamed or removed (Windows needs that)
    drop(file);
    if audio {
        publish(part, wanted).map(|path| Some(Recovered::Saved(path)))
    } else {
        fs::remove_file(part).map(|()| Some(Recovered::Removed(part.to_path_buf())))
    }
}

/// Whether `file` holds any audio: it decodes to at least one sample
fn has_audio(file: File, ext: Option<&str>) -> bool {
    match SymphoniaSource::new_with_hint(file, ext) {
        Ok(mut source) => source.next().is_some(),
        Err(_) => false,
    }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileAttributesW(name: *const u16) -> u32;
        fn SetFileAttributesW(name: *const u16, attributes: u32) -> i32;
    }

    /// Show the finished recording again (it was hidden while written)
    pub fn unhide(path: &Path) {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: plain Win32 calls with a NUL-terminated path; a failure
        // changes nothing
        unsafe {
            let attributes = GetFileAttributesW(wide.as_ptr());
            if attributes != INVALID_FILE_ATTRIBUTES && attributes & FILE_ATTRIBUTE_HIDDEN != 0 {
                SetFileAttributesW(wide.as_ptr(), attributes & !FILE_ATTRIBUTE_HIDDEN);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "radiotrope-part-{}-{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn part_name_is_hidden_beside_the_recording() {
        let path = Path::new("/music/BBC Radio 1 - 2026-11-22 20-11-11.mp3");
        assert_eq!(
            part_path(path),
            Path::new("/music/.BBC Radio 1 - 2026-11-22 20-11-11.mp3.part")
        );
    }

    #[test]
    fn only_our_names_are_ours() {
        assert_eq!(final_name(".A - 1.mp3.part"), Some("A - 1.mp3"));
        assert_eq!(final_name(".A.opus.part"), Some("A.opus"));
        assert_eq!(final_name(".A.wav.part"), Some("A.wav"));
        // Someone else's downloads, and our names without the dot or end
        assert_eq!(final_name(".movie.mkv.part"), None);
        assert_eq!(final_name("A.mp3.part"), None);
        assert_eq!(final_name(".A.mp3"), None);
        assert_eq!(final_name(".mp3.part"), None);
        assert_eq!(final_name(".A.mp3.part.part"), None);
    }

    #[test]
    fn taken_names_get_a_number() {
        let dir = temp_dir("taken");
        let wanted = dir.join("Test FM - 1.mp3");
        assert_eq!(free_path(&wanted), wanted);
        fs::write(&wanted, b"").unwrap();
        assert_eq!(free_path(&wanted), dir.join("Test FM - 1 (2).mp3"));
    }

    #[test]
    fn locked_leftovers_are_left_alone() {
        let dir = temp_dir("locked");
        let wanted = dir.join("Live FM - 1.wav");
        let _writing = create(&wanted).unwrap();
        let other = File::open(part_path(&wanted)).unwrap();
        if !matches!(other.try_lock(), Err(fs::TryLockError::WouldBlock)) {
            // A file system without locks: nothing to check
            return;
        }
        drop(other);
        assert!(recover_partial_recordings(&dir).is_empty());
        assert!(part_path(&wanted).exists());
    }

    #[test]
    fn files_that_are_not_ours_are_left_alone() {
        let dir = temp_dir("not-ours");
        for name in [".movie.mkv.part", "song.mp3.part", ".notes.txt"] {
            fs::write(dir.join(name), b"x").unwrap();
        }
        assert!(recover_partial_recordings(&dir).is_empty());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3);
    }
}
