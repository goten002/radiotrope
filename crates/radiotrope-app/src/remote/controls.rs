//! Recording and its settings, the equalizer, the Sleep Timer, the Scheduler and the accent,
//! for phones

use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};

use radiotrope::audio::PRESETS;
use radiotrope::config::eq::{FREQ_LABELS, MAX_GAIN_DB, MIN_GAIN_DB};
use radiotrope_app::data::schedule::{self, Action, End, Entry, MAX_MINUTES};
use radiotrope_app::data::settings::{parse_hex_rgb, RecordingFormat};
use radiotrope_app::data::types::url_to_id;

use super::server::{control_error, done, error, json, no_content, read_json, Body, Shared};
use super::state::{EqState, SleepState, State, ACCENT_SWATCHES, DEFAULT_ACCENT};
use crate::app::state::{AppCommand, RecordingSettingsChange};
use crate::control::{EntryRequest, RecordingStart, RecordingStop};

// -- Recording ---------------------------------------------------------------

#[derive(Serialize)]
struct RecordingStarted {
    /// "recording", "already" (it was), or "starting" (the state shows the
    /// file once it is under way)
    result: &'static str,
    /// The file's name, without its folder
    file: Option<String>,
}

/// `POST /v1/recording`: record the station playing, with the player's
/// recording settings
pub(super) async fn start_recording(shared: &Shared) -> Response<Body> {
    let file_name =
        |path: &std::path::Path| path.file_name().map(|n| n.to_string_lossy().into_owned());
    let (result, file) = match shared.control.start_recording(|| {}).await {
        Ok(RecordingStart::Started(path)) => ("recording", file_name(&path)),
        Ok(RecordingStart::Already(path)) => ("already", file_name(&path)),
        Ok(RecordingStart::Starting) => ("starting", None),
        Err(e) => return control_error(e),
    };
    json(StatusCode::OK, &RecordingStarted { result, file })
}

#[derive(Serialize)]
struct RecordingStopped {
    /// "saved", "not_recording" or "stopping"
    result: &'static str,
    /// The player's words, e.g. where the file was saved
    message: Option<String>,
}

/// `DELETE /v1/recording`: stop the recording and save the file
pub(super) async fn stop_recording(shared: &Shared) -> Response<Body> {
    let (result, message) = match shared.control.stop_recording(|| {}).await {
        Ok(RecordingStop::Saved(text)) => ("saved", Some(text)),
        Ok(RecordingStop::NotRecording) => ("not_recording", None),
        Ok(RecordingStop::Stopping(_)) => ("stopping", None),
        Err(e) => return control_error(e),
    };
    json(StatusCode::OK, &RecordingStopped { result, message })
}

// -- Recording settings ------------------------------------------------------

/// The bitrates the Recording Settings dialog offers besides Auto (0)
const RECORDING_BITRATES: [u32; 5] = [96, 128, 192, 256, 320];

/// What to change; what is left out stays. The folder is set only in the
/// window.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordingSettingsBody {
    /// "mp3", "opus" or "wav"
    #[serde(default)]
    format: Option<String>,
    /// kbps, one of the dialog's; 0 is Auto
    #[serde(default)]
    bitrate: Option<u32>,
    #[serde(default)]
    with_eq: Option<bool>,
}

/// `PUT /v1/recording-settings`: format, bitrate and the equalizer switch,
/// saved and shown in the window as if changed there
pub(super) async fn set_recording_settings(
    shared: &Shared,
    request: Request<Incoming>,
) -> Response<Body> {
    let body: RecordingSettingsBody = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let bad = |text: &str| error(StatusCode::BAD_REQUEST, "bad_request", text);
    let format = match body.format.as_deref() {
        None => None,
        Some(id @ ("mp3" | "opus" | "wav")) => Some(RecordingFormat::from_id(id)),
        Some(_) => return bad("format must be mp3, opus or wav"),
    };
    let bitrate = match body.bitrate {
        None => None,
        Some(0) => Some(None),
        Some(kbps) if RECORDING_BITRATES.contains(&kbps) => Some(Some(kbps)),
        Some(_) => return bad("bitrate must be 0 (Auto), 96, 128, 192, 256 or 320"),
    };
    let change = RecordingSettingsChange {
        format,
        bitrate,
        with_eq: body.with_eq,
    };
    shared.control.set_recording_settings(&change);
    if let Some(save) = &shared.window.recording_settings {
        save(change);
    }
    no_content()
}

// -- Equalizer ---------------------------------------------------------------

#[derive(Serialize)]
struct Preset {
    name: &'static str,
    /// The heading it sits under in the preset menu; none for Flat
    group: Option<&'static str>,
    gains: [f32; 10],
    /// The preamp picking it sets
    preamp: f32,
}

#[derive(Serialize)]
struct Equalizer {
    #[serde(flatten)]
    now: EqState,
    /// The bands' labels in Hz, as the window shows them ("40" .. "12K")
    bands: [&'static str; 10],
    min_db: f32,
    max_db: f32,
    /// In the window's menu order
    presets: Vec<Preset>,
}

/// `GET /v1/eq`: the equalizer now, and what it offers
pub(super) fn eq(shared: &Shared) -> Response<Body> {
    let now = State::from_snapshot(&shared.control.snapshot(), 0).eq;
    json(
        StatusCode::OK,
        &Equalizer {
            now,
            bands: FREQ_LABELS,
            min_db: MIN_GAIN_DB,
            max_db: MAX_GAIN_DB,
            presets: PRESETS
                .iter()
                .map(|p| Preset {
                    name: p.name,
                    group: p.group.title(),
                    gains: p.gains,
                    preamp: p.preamp_db(),
                })
                .collect(),
        },
    )
}

/// What to change; what is left out stays. A preset brings its own
/// preamp, unless one is given with it.
#[derive(Deserialize)]
struct EqBody {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    gains: Option<[f32; 10]>,
    #[serde(default)]
    preamp: Option<f32>,
}

/// `PUT /v1/eq`
pub(super) async fn set_eq(shared: &Shared, request: Request<Incoming>) -> Response<Body> {
    let body: EqBody = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let bad = |text: &str| error(StatusCode::BAD_REQUEST, "bad_request", text);
    let mut commands = Vec::new();
    if let Some(on) = body.enabled {
        commands.push(AppCommand::SetEqEnabled(on));
    }
    match (body.preset, body.gains) {
        (Some(_), Some(_)) => return bad("Give preset or gains, not both"),
        (Some(name), None) => match radiotrope::audio::find_preset(&name) {
            Some(preset) => commands.push(AppCommand::SetEqPreset(preset.name.to_string())),
            None => return bad(&format!("No preset named {name:?}")),
        },
        (None, Some(gains)) => {
            if gains.iter().any(|g| !g.is_finite()) {
                return bad("gains must be numbers");
            }
            commands.push(AppCommand::SetEqGains(
                gains.map(|g| g.clamp(MIN_GAIN_DB, MAX_GAIN_DB)),
            ));
        }
        (None, None) => {}
    }
    if let Some(db) = body.preamp {
        if !db.is_finite() {
            return bad("preamp must be a number");
        }
        commands.push(AppCommand::SetEqPreamp(db.clamp(MIN_GAIN_DB, MAX_GAIN_DB)));
    }
    for command in commands {
        if let Err(e) = shared.control.send(command) {
            return control_error(e);
        }
    }
    no_content()
}

// -- Sleep Timer -------------------------------------------------------------

#[derive(Deserialize)]
struct SleepBody {
    /// Stop this many minutes from now; 0 turns the timer off
    minutes: u32,
    /// Lower the volume over the last minute (default on)
    #[serde(default)]
    fade: Option<bool>,
}

/// `PUT /v1/sleep-timer`
pub(super) async fn sleep_timer(shared: &Shared, request: Request<Incoming>) -> Response<Body> {
    let body: SleepBody = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    if body.minutes > MAX_MINUTES {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            &format!("minutes must be from 0 to {MAX_MINUTES}"),
        );
    }
    let minutes = (body.minutes > 0).then_some(body.minutes);
    done(
        shared
            .control
            .set_sleep_timer(minutes, body.fade.unwrap_or(true)),
    )
}

// -- Scheduler ---------------------------------------------------------------

#[derive(Serialize)]
struct EntryStation {
    name: String,
    url: String,
    /// Where to get its logo from this player, when it has one
    logo: Option<String>,
    country: Option<String>,
}

/// One Scheduler entry as phones see it
#[derive(Serialize)]
struct EntryItem {
    id: u64,
    enabled: bool,
    /// "play", "record" or "stop"
    action: &'static str,
    station: Option<EntryStation>,
    /// "HH:MM"
    start: String,
    /// Days it repeats ("mon".."sun"); empty for a one-off
    days: Vec<&'static str>,
    /// A one-off's day, "YYYY-MM-DD"
    date: Option<String>,
    /// {"kind": "never"}, {"kind": "after", "minutes": 60} or
    /// {"kind": "at", "time": "22:00"}
    end: End,
    /// 0 to 100; none keeps the player's volume
    volume: Option<u8>,
    fade: bool,
    fallback: bool,
    /// e.g. "Play Jazz FM"
    title: String,
    /// e.g. "Weekdays · for 1 h · fade in"
    summary: String,
    /// When it next comes round, "YYYY-MM-DD HH:MM" local time
    next: Option<String>,
    /// The same, in words: "Tomorrow 07:00"
    next_label: Option<String>,
}

fn entry_item(entry: &Entry, now: &chrono::DateTime<chrono::Local>) -> EntryItem {
    const IDS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let next = entry.next_start(now);
    EntryItem {
        id: entry.id,
        enabled: entry.enabled,
        action: entry.action.id(),
        station: entry.station.as_ref().map(|s| EntryStation {
            name: s.name.clone(),
            url: s.url.clone(),
            logo: s
                .logo_url
                .as_ref()
                .map(|_| format!("/v1/logos/{}", url_to_id(&s.url))),
            country: s.country.clone(),
        }),
        start: entry.start.to_string(),
        days: entry
            .days
            .flags()
            .iter()
            .zip(IDS)
            .filter(|(on, _)| **on)
            .map(|(_, id)| id)
            .collect(),
        date: entry
            .date
            .filter(|_| entry.days.is_once())
            .map(|d| d.format("%Y-%m-%d").to_string()),
        end: entry.end,
        volume: entry
            .volume
            .map(|v| (v * 100.0).round().clamp(0.0, 100.0) as u8),
        fade: entry.fade,
        fallback: entry.fallback,
        title: entry.title(),
        summary: entry.details(),
        next: next
            .as_ref()
            .map(|at| at.format("%Y-%m-%d %H:%M").to_string()),
        next_label: next.as_ref().map(|at| schedule::when_label(at, now)),
    }
}

#[derive(Serialize)]
struct ScheduleList {
    /// The `schedule_rev` of the state these are from
    rev: u64,
    /// Ordered by start time
    entries: Vec<EntryItem>,
    sleep_timer: Option<SleepState>,
}

/// `GET /v1/schedule`
pub(super) fn schedule(shared: &Shared) -> Response<Body> {
    let snapshot = shared.control.snapshot();
    let state = State::from_snapshot(&snapshot, 0);
    let now = chrono::Local::now();
    let mut entries: Vec<&Entry> = snapshot.schedule.iter().collect();
    entries.sort_by_key(|e| (e.start, e.id));
    json(
        StatusCode::OK,
        &ScheduleList {
            rev: state.schedule_rev,
            entries: entries.into_iter().map(|e| entry_item(e, &now)).collect(),
            sleep_timer: state.sleep_timer,
        },
    )
}

/// An entry as phones send it, as the Scheduler's form has it
#[derive(Deserialize)]
struct EntryBody {
    action: Action,
    start: String,
    #[serde(default)]
    days: Vec<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    favorite_id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    end_after_minutes: Option<f64>,
    #[serde(default)]
    end_at: Option<String>,
    #[serde(default)]
    volume: Option<f64>,
    #[serde(default)]
    fade: bool,
    #[serde(default)]
    fallback: Option<bool>,
    /// Only when replacing; a new entry is on
    #[serde(default)]
    enabled: Option<bool>,
}

impl From<EntryBody> for EntryRequest {
    fn from(b: EntryBody) -> Self {
        EntryRequest {
            action: b.action,
            start: b.start,
            days: b.days,
            date: b.date,
            favorite_id: b.favorite_id,
            url: b.url,
            name: b.name,
            end_after_minutes: b.end_after_minutes,
            end_at: b.end_at,
            volume: b.volume,
            fade: b.fade,
            fallback: b.fallback,
        }
    }
}

/// Check and save an entry; answers with it as saved
async fn save_entry(
    shared: &Shared,
    body: EntryBody,
    replacing: Option<Entry>,
    status: StatusCode,
) -> Response<Body> {
    let enabled = body
        .enabled
        .or(replacing.as_ref().map(|e| e.enabled))
        .unwrap_or(true);
    let mut entry = match shared.control.build_entry(body.into()).await {
        Ok(entry) => entry,
        Err(e) => return bad_entry(e),
    };
    if let Some(old) = replacing {
        entry.id = old.id;
    }
    entry.enabled = enabled;
    let id = match shared.control.save_entry(entry).await {
        Ok(id) => id,
        Err(e) => return control_error(e),
    };
    match shared.control.entry(id) {
        Ok(saved) => json(status, &entry_item(&saved, &chrono::Local::now())),
        Err(e) => control_error(e),
    }
}

/// The form's mistakes are the phone's: 400, except an unknown favorite
fn bad_entry(e: crate::control::Error) -> Response<Body> {
    match e {
        crate::control::Error::Failed(text) => error(StatusCode::BAD_REQUEST, "bad_request", &text),
        other => control_error(other),
    }
}

/// `POST /v1/schedule`
pub(super) async fn add_entry(shared: &Shared, request: Request<Incoming>) -> Response<Body> {
    match read_json::<EntryBody>(request).await {
        Ok(body) => save_entry(shared, body, None, StatusCode::CREATED).await,
        Err(response) => *response,
    }
}

/// `PUT /v1/schedule/{id}`: replace the entry, keeping its id
pub(super) async fn replace_entry(
    shared: &Shared,
    id: &str,
    request: Request<Incoming>,
) -> Response<Body> {
    let old = match entry_id(id).map(|id| shared.control.entry(id)) {
        Some(Ok(old)) => old,
        Some(Err(e)) => return control_error(e),
        None => return no_entry(id),
    };
    match read_json::<EntryBody>(request).await {
        Ok(body) => save_entry(shared, body, Some(old), StatusCode::OK).await,
        Err(response) => *response,
    }
}

/// `DELETE /v1/schedule/{id}`
pub(super) async fn remove_entry(shared: &Shared, id: &str) -> Response<Body> {
    match entry_id(id) {
        Some(id) => done(shared.control.remove_entry(id).await),
        None => no_entry(id),
    }
}

#[derive(Deserialize)]
struct EnabledBody {
    enabled: bool,
}

/// `PUT /v1/schedule/{id}/enabled`
pub(super) async fn enable_entry(
    shared: &Shared,
    id: &str,
    request: Request<Incoming>,
) -> Response<Body> {
    let Some(id) = entry_id(id) else {
        return no_entry(id);
    };
    match read_json::<EnabledBody>(request).await {
        Ok(body) => done(shared.control.set_entry_enabled(id, body.enabled).await),
        Err(response) => *response,
    }
}

fn entry_id(id: &str) -> Option<u64> {
    id.parse::<u64>().ok().filter(|id| *id > 0)
}

fn no_entry(id: &str) -> Response<Body> {
    error(
        StatusCode::NOT_FOUND,
        "no_entry",
        &format!("No schedule entry with id {id}"),
    )
}

// -- Accent ------------------------------------------------------------------

#[derive(Serialize)]
struct Swatch {
    /// "#rrggbb"
    color: &'static str,
    name: &'static str,
}

#[derive(Serialize)]
struct Appearance {
    /// "#rrggbb"
    accent: String,
    /// What Restore Default goes back to
    default: &'static str,
    /// The Accent Color dialog's swatches, in its order
    swatches: Vec<Swatch>,
}

/// `GET /v1/appearance`
pub(super) fn appearance(shared: &Shared) -> Response<Body> {
    json(
        StatusCode::OK,
        &Appearance {
            accent: State::from_snapshot(&shared.control.snapshot(), 0).accent,
            default: DEFAULT_ACCENT,
            swatches: ACCENT_SWATCHES
                .iter()
                .map(|(color, name)| Swatch { color, name })
                .collect(),
        },
    )
}

#[derive(Deserialize)]
struct AppearanceBody {
    /// "#rrggbb"
    accent: String,
}

/// `PUT /v1/appearance`: use this accent, in the window too
pub(super) async fn set_appearance(shared: &Shared, request: Request<Incoming>) -> Response<Body> {
    let body: AppearanceBody = match read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let Some((r, g, b)) = parse_hex_rgb(body.accent.trim()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "accent must be a colour like #3584e4",
        );
    };
    let hex = format!("#{r:02x}{g:02x}{b:02x}");
    shared.control.set_accent(hex.clone());
    if let Some(show) = &shared.window.accent {
        show(hex);
    }
    no_content()
}
