//! Recordings on a phone: list, download and delete the files in the
//! recording folder
//!
//! Off until the user turns on Share recordings in Tools > Remote Control,
//! and then for every paired phone. Phones never name a path: a file is
//! asked for by an id the player gives in its list, and every request
//! lists the folder again and looks the id up there. Only files named as
//! recordings are listed (`<station> - <date> <time>.mp3|opus|wav`), only
//! regular files directly in the folder, and they are opened without
//! following links, so a phone can reach nothing else on the computer.
//! The file being recorded is listed but can't be downloaded or deleted.
//! Agents (MCP) have none of this.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::Serialize;
use sha2::Sha256;
use tokio_util::sync::CancellationToken;

use radiotrope_app::data::recordings;

use super::server::{error, json, no_content, Body, Events, Shared};

/// Downloads one phone may run at once
const DOWNLOADS_PER_PHONE: usize = 1;
/// Downloads all phones together may run at once
const DOWNLOADS_TOTAL: usize = 2;
/// Bytes read and sent at a time
const CHUNK: usize = 64 * 1024;
/// A file written to this recently may still be being written (by the
/// player's command line, say): it is treated as being recorded
const STILL_WRITING: Duration = Duration::from_secs(10);
/// How many of the last downloads and deletions the dialog keeps
const ACTIVITY_KEPT: usize = 20;
/// How many files the player finished recording it remembers as finished
const FINISHED_KEPT: usize = 8;
/// Formats the player records, and what phones are told they are
const FORMATS: &[(&str, &str)] = &[
    ("mp3", "audio/mpeg"),
    ("opus", "audio/ogg"),
    ("wav", "audio/wav"),
];

/// What the server keeps about recordings on phones
pub struct Recordings {
    /// The user's switch: paired phones may see the recordings
    on: AtomicBool,
    /// Signs the file ids, so they stand for a name only in this run
    key: [u8; 32],
    /// Counts deletions and the switch going on or off
    changes: AtomicU64,
    downloads: Mutex<HashMap<String, usize>>,
    activity: Mutex<VecDeque<Activity>>,
    /// The file the player was recording when last seen, and the files it
    /// finished since: closed, so not "still being written" however new
    recorded: Mutex<(Option<PathBuf>, VecDeque<PathBuf>)>,
}

/// A phone downloaded or deleted a recording
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub device: String,
    pub deleted: bool,
    pub file: String,
    /// Unix time
    pub at: i64,
}

impl Default for Recordings {
    fn default() -> Self {
        let mut key = [0u8; 32];
        if getrandom::fill(&mut key).is_err() {
            // Ids are then guessable from the names, which phones see
            // anyway: they still name only listed files
            key = [0x5a; 32];
        }
        Self {
            on: AtomicBool::new(false),
            key,
            changes: AtomicU64::new(0),
            downloads: Mutex::default(),
            activity: Mutex::default(),
            recorded: Mutex::default(),
        }
    }
}

impl Recordings {
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    /// The user turned Share recordings on or off
    pub fn set_on(&self, on: bool) {
        if self.on.swap(on, Ordering::SeqCst) != on {
            self.changes.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Changes whenever the list may have changed: phones list again. Made
    /// from what changes it (deletions, the switch, the folder and the file
    /// being recorded), and kept within what a phone's JSON reads exactly.
    pub fn rev(&self, folder: &Path, recording: Option<&Path>) -> u64 {
        use std::hash::{Hash, Hasher};
        self.saw_recording(recording);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.changes.load(Ordering::SeqCst).hash(&mut hasher);
        self.is_on().hash(&mut hasher);
        folder.hash(&mut hasher);
        recording.hash(&mut hasher);
        hasher.finish() & ((1 << 53) - 1)
    }

    /// Note the file the player records now: the one it recorded before,
    /// if another, is finished
    fn saw_recording(&self, recording: Option<&Path>) {
        let mut recorded = self.recorded.lock().unwrap_or_else(|e| e.into_inner());
        let (last, finished) = &mut *recorded;
        if last.as_deref() == recording {
            return;
        }
        if let Some(done) = last.take() {
            finished.retain(|f| *f != done);
            finished.push_back(done);
            if finished.len() > FINISHED_KEPT {
                finished.pop_front();
            }
        }
        *last = recording.map(Path::to_path_buf);
    }

    /// The player finished recording `path` itself
    fn finished(&self, path: &Path) -> bool {
        let recorded = self.recorded.lock().unwrap_or_else(|e| e.into_inner());
        recorded.1.iter().any(|f| f == path)
    }

    /// The last downloads and deletions, newest first
    pub fn activity(&self) -> Vec<Activity> {
        self.activity_list().iter().rev().cloned().collect()
    }

    fn activity_list(&self) -> std::sync::MutexGuard<'_, VecDeque<Activity>> {
        self.activity.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn note(&self, device: String, deleted: bool, file: &str) {
        let mut list = self.activity_list();
        list.push_back(Activity {
            device,
            deleted,
            file: file.to_string(),
            at: unix_now(),
        });
        while list.len() > ACTIVITY_KEPT {
            list.pop_front();
        }
    }

    /// The id phones use for the file `name`
    fn id(&self, name: &str) -> String {
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(&self.key).expect("HMAC takes any key length");
        mac.update(name.as_bytes());
        hex(&mac.finalize().into_bytes()[..16])
    }

    /// A place for one more download by `device_id`, held until dropped;
    /// `None` when that phone or all phones together have their most
    fn take_download(self: &Arc<Self>, device_id: &str) -> Option<DownloadPlace> {
        let mut downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
        let total: usize = downloads.values().sum();
        let mine = downloads.get(device_id).copied().unwrap_or(0);
        if total >= DOWNLOADS_TOTAL || mine >= DOWNLOADS_PER_PHONE {
            return None;
        }
        *downloads.entry(device_id.to_string()).or_default() += 1;
        Some(DownloadPlace {
            recordings: self.clone(),
            device_id: device_id.to_string(),
        })
    }
}

/// A running download's place; dropping it frees the place
struct DownloadPlace {
    recordings: Arc<Recordings>,
    device_id: String,
}

impl Drop for DownloadPlace {
    fn drop(&mut self) {
        let mut downloads = self
            .recordings
            .downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(n) = downloads.get_mut(&self.device_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                downloads.remove(&self.device_id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The folder
// ---------------------------------------------------------------------------

/// A recording in the folder
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub id: String,
    /// The file's name, e.g. "Jazz FM - 2026-10-08 20-15-03.mp3"
    pub name: String,
    pub size: u64,
    /// Unix time it was last written
    pub modified: i64,
    /// "mp3", "opus" or "wav"
    pub format: &'static str,
    /// Being recorded (or written) now: it can't be downloaded or deleted
    pub recording: bool,
    /// What `If-Match` sends to delete it, or to resume its download
    pub etag: String,
}

/// The format of a file named as the player names recordings,
/// `<station> - YYYY-MM-DD HH-MM-SS[ (n)].<ext>`; `None` for any other name
pub fn recording_format(name: &str) -> Option<&'static str> {
    if name.starts_with('.') || name.contains(['/', '\\']) {
        return None;
    }
    let (stem, ext) = name.rsplit_once('.')?;
    let format = FORMATS.iter().find(|(e, _)| *e == ext)?.0;
    // A number the player adds when the name is taken
    let stem = match stem.strip_suffix(')').and_then(|s| s.rsplit_once(" (")) {
        Some((rest, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => rest,
        _ => stem,
    };
    // " - 2026-10-08 20-15-03": 22 bytes, all ASCII
    let (station, time) = stem.split_at_checked(stem.len().checked_sub(22)?)?;
    let shape = b" - 0000-00-00 00-00-00";
    let fits = time.bytes().zip(shape).all(|(b, s)| match s {
        b'0' => b.is_ascii_digit(),
        s => b == *s,
    });
    (fits && !station.is_empty()).then_some(format)
}

/// What phones compare to tell a file has changed: its size and the time
/// it was last written
fn etag(meta: &Metadata) -> String {
    let nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("\"{:x}-{nanos:x}\"", meta.len())
}

/// The recordings in `folder`, newest first. `recording` is the file
/// being recorded now.
fn list(
    recordings: &Recordings,
    folder: &Path,
    recording: Option<&Path>,
) -> io::Result<Vec<Entry>> {
    recordings.saw_recording(recording);
    let now = SystemTime::now();
    let mut entries = Vec::new();
    let read = match fs::read_dir(folder) {
        Ok(read) => read,
        // Nothing recorded yet
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(entries),
        Err(e) => return Err(e),
    };
    for item in read.flatten() {
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        let Some(format) = recording_format(&name) else {
            continue;
        };
        // Not a link, folder or device: plain files only
        let Ok(meta) = fs::symlink_metadata(item.path()) else {
            continue;
        };
        if !meta.file_type().is_file() || is_reparse_point(&meta) {
            continue;
        }
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        let path = item.path();
        let being_recorded = recording.is_some_and(|r| r == path)
            || (!recordings.finished(&path)
                && now
                    .duration_since(modified)
                    .map_or(true, |ago| ago < STILL_WRITING));
        entries.push(Entry {
            id: recordings.id(&name),
            name,
            size: meta.len(),
            modified: modified
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            format,
            recording: being_recorded,
            etag: etag(&meta),
        });
    }
    entries.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
    Ok(entries)
}

#[cfg(windows)]
fn is_reparse_point(meta: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &Metadata) -> bool {
    false
}

/// Open `path` to read without following a link put there since it was
/// listed; only a plain file opens
fn open_plain(path: &Path) -> io::Result<(File, Metadata)> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Non-blocking too, so a pipe swapped in can't hang the open
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Opens a link itself rather than what it points to; refused below
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || is_reparse_point(&meta) {
        return Err(io::Error::other("not a plain file"));
    }
    Ok((file, meta))
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// The recording folder and the file being recorded, as the window has them
fn where_now(shared: &Shared) -> (PathBuf, Option<PathBuf>) {
    let snapshot = shared.control.snapshot();
    (
        recordings::folder(snapshot.recording_setup.dir.as_deref()),
        snapshot.recording.map(|r| r.path),
    )
}

fn sharing_off() -> Response<Body> {
    error(
        StatusCode::FORBIDDEN,
        "recordings_off",
        "Recordings aren't shared. Turn on Share recordings in Tools > Remote Control on the player.",
    )
}

fn not_found() -> Response<Body> {
    error(
        StatusCode::NOT_FOUND,
        "no_recording",
        "That recording isn't there anymore",
    )
}

fn being_recorded() -> Response<Body> {
    error(
        StatusCode::CONFLICT,
        "recording",
        "This recording is still being made. Try again once it stops.",
    )
}

fn changed() -> Response<Body> {
    error(
        StatusCode::PRECONDITION_FAILED,
        "changed",
        "The recording changed on the player; list the recordings again",
    )
}

fn unreadable(e: io::Error) -> Response<Body> {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed",
        &format!("Can't read the recording folder: {e}"),
    )
}

/// Lists the folder off the async thread
async fn listed(shared: &Shared) -> io::Result<(PathBuf, Vec<Entry>)> {
    let (folder, recording) = where_now(shared);
    let recordings = shared.recordings.clone();
    tokio::task::spawn_blocking(move || {
        let entries = list(&recordings, &folder, recording.as_deref())?;
        Ok((folder, entries))
    })
    .await
    .unwrap_or_else(|e| Err(io::Error::other(e.to_string())))
}

#[derive(Serialize)]
struct Listing {
    rev: u64,
    recordings: Vec<Entry>,
}

/// `GET /v1/recordings`: the recordings, newest first
pub(super) async fn list_all(shared: &Shared) -> Response<Body> {
    if !shared.recordings.is_on() {
        return sharing_off();
    }
    let (folder, recording) = where_now(shared);
    let rev = shared.recordings.rev(&folder, recording.as_deref());
    match listed(shared).await {
        Ok((_, recordings)) => json(StatusCode::OK, &Listing { rev, recordings }),
        Err(e) => unreadable(e),
    }
}

/// `GET /v1/recordings/<id>`: the file, or the part a `Range` header asks
/// for. `If-Match` refuses a file that changed since the phone listed it.
pub(super) async fn download(
    shared: &Shared,
    device_id: &str,
    id: &str,
    request: &Request<Incoming>,
    cancel: CancellationToken,
) -> Response<Body> {
    if !shared.recordings.is_on() {
        return sharing_off();
    }
    let (folder, entries) = match listed(shared).await {
        Ok(listed) => listed,
        Err(e) => return unreadable(e),
    };
    let Some(entry) = entries.into_iter().find(|e| e.id == id) else {
        return not_found();
    };
    if entry.recording {
        return being_recorded();
    }
    if let Some(wanted) = header(request, http::header::IF_MATCH) {
        if wanted != entry.etag {
            return changed();
        }
    }
    let Some(place) = shared.recordings.take_download(device_id) else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "busy",
            "Another download is running. Try again when it ends.",
        );
    };
    let path = folder.join(&entry.name);
    let opened = tokio::task::spawn_blocking(move || open_plain(&path)).await;
    let (mut file, meta) = match opened {
        Ok(Ok(opened)) => opened,
        _ => return not_found(),
    };
    // Swapped or written to since it was listed
    if etag(&meta) != entry.etag {
        return changed();
    }
    let size = meta.len();
    let range = match header(request, http::header::RANGE) {
        None => None,
        Some(value) => match parse_range(&value, size) {
            Some(range) => Some(range),
            None => {
                let mut response = error(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "bad_range",
                    "The file doesn't have that part",
                );
                if let Ok(v) = http::HeaderValue::from_str(&format!("bytes */{size}")) {
                    response
                        .headers_mut()
                        .insert(http::header::CONTENT_RANGE, v);
                }
                return response;
            }
        },
    };
    let (start, end) = range.unwrap_or((0, size.saturating_sub(1)));
    let length = if size == 0 { 0 } else { end - start + 1 };
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        return not_found();
    }
    if start == 0 {
        // A download beginning, not a resume
        let device = shared
            .device_name(device_id)
            .unwrap_or_else(|| "A phone".into());
        shared.recordings.note(device, false, &entry.name);
        shared.changed();
    }

    // Read on a plain thread and hand the pieces to the connection
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    let checks = shared.clone();
    let device_id = device_id.to_string();
    let spawned = std::thread::Builder::new()
        .name("recording-sender".into())
        .spawn(move || {
            let _place = place;
            let mut left = length;
            let mut buffer = vec![0u8; CHUNK];
            while left > 0 {
                // Still wanted: the phone is connected and paired, sharing
                // is on, and Remote Control is on
                if cancel.is_cancelled()
                    || !checks.recordings.is_on()
                    || checks.device_name(&device_id).is_none()
                {
                    return;
                }
                let want = buffer.len().min(left as usize);
                let read = match file.read(&mut buffer[..want]) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                left -= read as u64;
                if tx
                    .blocking_send(Bytes::copy_from_slice(&buffer[..read]))
                    .is_err()
                {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "failed",
            &format!("Can't send the recording: {e}"),
        );
    }

    let mut response = Response::new(Events(rx).boxed());
    if range.is_some() {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    let content_type = FORMATS
        .iter()
        .find(|(e, _)| *e == entry.format)
        .map_or("application/octet-stream", |(_, t)| t);
    let headers = response.headers_mut();
    let mut set = |name: http::HeaderName, value: String| {
        if let Ok(v) = http::HeaderValue::from_str(&value) {
            headers.insert(name, v);
        }
    };
    set(http::header::CONTENT_TYPE, content_type.into());
    set(http::header::CONTENT_LENGTH, length.to_string());
    set(http::header::ACCEPT_RANGES, "bytes".into());
    set(http::header::ETAG, entry.etag.clone());
    set(http::header::CACHE_CONTROL, "no-store".into());
    set(
        http::header::CONTENT_DISPOSITION,
        format!(
            "attachment; filename*=UTF-8''{}",
            percent_encode(&entry.name)
        ),
    );
    if range.is_some() {
        set(
            http::header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{size}"),
        );
    }
    response
}

/// `DELETE /v1/recordings/<id>` with `If-Match`: removes the file, if it
/// is still the one the phone listed
pub(super) async fn delete(
    shared: &Shared,
    device_id: &str,
    id: &str,
    request: &Request<Incoming>,
) -> Response<Body> {
    if !shared.recordings.is_on() {
        return sharing_off();
    }
    let Some(wanted) = header(request, http::header::IF_MATCH) else {
        return error(
            StatusCode::PRECONDITION_REQUIRED,
            "if_match_needed",
            "Send the recording's ETag in If-Match to delete it",
        );
    };
    let (folder, entries) = match listed(shared).await {
        Ok(listed) => listed,
        Err(e) => return unreadable(e),
    };
    let Some(entry) = entries.into_iter().find(|e| e.id == id) else {
        return not_found();
    };
    if entry.recording {
        return being_recorded();
    }
    if wanted != entry.etag {
        return changed();
    }
    let path = folder.join(&entry.name);
    let etag_listed = entry.etag.clone();
    let removed = tokio::task::spawn_blocking(move || {
        // Checked once more right before: still the same plain file
        let meta = fs::symlink_metadata(&path)?;
        if !meta.file_type().is_file() || is_reparse_point(&meta) || etag(&meta) != etag_listed {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "changed"));
        }
        fs::remove_file(&path)
    })
    .await
    .unwrap_or_else(|e| Err(io::Error::other(e.to_string())));
    match removed {
        Ok(()) => {
            let device = shared
                .device_name(device_id)
                .unwrap_or_else(|| "A phone".into());
            shared.recordings.note(device, true, &entry.name);
            shared.recordings.changes.fetch_add(1, Ordering::SeqCst);
            shared.changed();
            no_content()
        }
        Err(e) if e.kind() == io::ErrorKind::InvalidData => changed(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => not_found(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed",
            &format!("Can't delete the recording: {e}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------

fn header(request: &Request<Incoming>, name: http::HeaderName) -> Option<String> {
    request
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
}

/// The first and last byte a `Range: bytes=…` header asks for in a file of
/// `size` bytes: one range only (`a-b`, `a-` or `-n`); `None` when it
/// can't be served
pub fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') || size == 0 {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        // The last n bytes
        let n: u64 = last.parse().ok()?;
        if n == 0 {
            return None;
        }
        return Some((size.saturating_sub(n), size - 1));
    }
    let start: u64 = first.parse().ok()?;
    let end = if last.is_empty() {
        size - 1
    } else {
        last.parse::<u64>().ok()?.min(size - 1)
    };
    (start <= end).then_some((start, end))
}

/// `name` for a `filename*=UTF-8''…` parameter
fn percent_encode(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "radiotrope-remote-recs-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Written a minute ago, so it isn't taken for one being written
    fn old_file(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        let file = File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
    }

    #[test]
    fn only_names_the_player_makes_are_recordings() {
        let ok = |n: &str| recording_format(n);
        assert_eq!(ok("Jazz FM - 2026-10-08 20-15-03.mp3"), Some("mp3"));
        assert_eq!(ok("Jazz FM - 2026-10-08 20-15-03 (2).opus"), Some("opus"));
        assert_eq!(ok("語 - 2026-10-08 20-15-03.wav"), Some("wav"));
        assert_eq!(ok("Jazz FM - 2026-10-08 20-15-03.txt"), None);
        assert_eq!(ok("Jazz FM - 2026-10-08 20-15-03.MP3"), None);
        assert_eq!(ok(" - 2026-10-08 20-15-03.mp3"), None);
        assert_eq!(ok("Jazz FM - 2026-10-08.mp3"), None);
        assert_eq!(ok("Jazz FM - 2026-10-08 20-15-03 ().mp3"), None);
        assert_eq!(ok(".hidden - 2026-10-08 20-15-03.mp3"), None);
        assert_eq!(ok("passwd"), None);
        assert_eq!(ok("../x - 2026-10-08 20-15-03.mp3"), None);
        assert_eq!(ok("é - 2026-10-08 20-15-0é.mp3"), None);
    }

    #[test]
    fn the_list_has_recordings_only_newest_first() {
        let dir = temp_dir("list");
        let recordings = Recordings::default();
        old_file(&dir.join("A - 2026-10-01 10-00-00.mp3"), b"aaa");
        old_file(&dir.join("notes.txt"), b"x");
        fs::create_dir(dir.join("B - 2026-10-01 10-00-00.mp3")).unwrap();
        // Written just now: maybe still being written
        fs::write(dir.join("C - 2026-10-02 10-00-00.opus"), b"cc").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", dir.join("D - 2026-10-01 10-00-00.wav")).unwrap();
        let entries = list(&recordings, &dir, None).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "C - 2026-10-02 10-00-00.opus",
                "A - 2026-10-01 10-00-00.mp3"
            ]
        );
        assert!(entries[0].recording);
        assert!(!entries[1].recording);
        assert_eq!(entries[1].size, 3);
        assert_eq!(entries[1].id.len(), 32);
        // Being recorded by the player
        let a = dir.join("A - 2026-10-01 10-00-00.mp3");
        assert!(list(&recordings, &dir, Some(&a)).unwrap()[1].recording);
        // No folder yet: nothing recorded
        assert!(list(&recordings, &dir.join("none"), None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_recording_the_player_stopped_is_finished_at_once() {
        let dir = temp_dir("stopped");
        let recordings = Recordings::default();
        let file = dir.join("C - 2026-10-02 10-00-00.opus");
        fs::write(&file, b"cc").unwrap();
        let rev = recordings.rev(&dir, Some(&file));
        assert!(list(&recordings, &dir, Some(&file)).unwrap()[0].recording);
        // Stopped: the state the phones get says so, and the list too,
        // although the file was written just now
        assert_ne!(recordings.rev(&dir, None), rev);
        assert!(!list(&recordings, &dir, None).unwrap()[0].recording);
        // Another new file the player didn't write may still be written
        fs::write(dir.join("D - 2026-10-02 10-00-01.mp3"), b"d").unwrap();
        let entries = list(&recordings, &dir, None).unwrap();
        assert!(entries
            .iter()
            .any(|e| e.name.starts_with("D ") && e.recording));
    }

    #[test]
    fn ids_are_the_players_own() {
        let a = Recordings::default();
        let b = Recordings::default();
        let name = "A - 2026-10-01 10-00-00.mp3";
        assert_eq!(a.id(name), a.id(name));
        assert_ne!(a.id(name), b.id(name));
        assert_ne!(a.id(name), a.id("B - 2026-10-01 10-00-00.mp3"));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_does_not_open() {
        let dir = temp_dir("link");
        let target = dir.join("target");
        old_file(&target, b"secret");
        let link = dir.join("A - 2026-10-01 10-00-00.mp3");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_plain(&link).is_err());
        assert!(open_plain(&target).is_ok());
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=900-5000", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-5000", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-1", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=0-", 0), None);
    }

    #[test]
    fn download_places_are_limited() {
        let recordings = Arc::new(Recordings::default());
        let first = recordings.take_download("phone1").unwrap();
        assert!(recordings.take_download("phone1").is_none());
        let second = recordings.take_download("phone2").unwrap();
        assert!(recordings.take_download("phone3").is_none());
        drop(first);
        assert!(recordings.take_download("phone1").is_some());
        drop(second);
    }

    #[test]
    fn activity_keeps_the_last_ones_newest_first() {
        let recordings = Recordings::default();
        for i in 0..25 {
            recordings.note("Pixel".into(), i % 2 == 0, &format!("f{i}"));
        }
        let list = recordings.activity();
        assert_eq!(list.len(), ACTIVITY_KEPT);
        assert_eq!(list[0].file, "f24");
        assert!(list[0].deleted);
    }

    #[test]
    fn file_names_are_encoded_for_the_header() {
        assert_eq!(
            percent_encode("Jazz FM - 2026.mp3"),
            "Jazz%20FM%20-%202026.mp3"
        );
        assert_eq!(percent_encode("é\"\r"), "%C3%A9%22%0D");
    }
}
