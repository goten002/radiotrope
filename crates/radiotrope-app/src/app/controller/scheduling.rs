//! The controller's side of the schedule: runs the entries that come
//! round, the sleep timer, and the fades of both
//!
//! The rules of what is due are in `data::schedule`; this does what they
//! decide with the same calls the window's buttons make. Times are read
//! from the wall clock about once a second: a monotonic clock stops while
//! the computer sleeps, and would run a timer set before a suspend late.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeDelta};

use radiotrope::audio::{PlaybackState, RecordingStatus};
use radiotrope_app::data::recordings;
use radiotrope_app::data::schedule::{
    self, Action, Decision, Entry, LastRun, Outcome, Running, ScheduleFile, ScheduledStation,
};
use radiotrope_app::data::storage;
use radiotrope_app::data::types::Station;

use super::alarm_tone::AlarmTone;
use super::AppController;
use crate::app::state::{NextRun, NoticeKind, ScheduleReply, ScheduledNow, SleepTimerInfo};

/// How long an alarm's volume takes to rise from silence
pub const FADE_IN: Duration = Duration::from_secs(30);

/// How long the sleep timer and a bedtime stop take to fade out. A
/// bedtime stop fades over the minute before it, so playback ends on time.
pub const FADE_OUT_SECS: i64 = 60;

/// Wait between tries to start an entry's station that failed
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// How long an alarm with no end is looked after: its station tried again
/// if it fails. After that it is the user's.
pub const ALARM_WATCH_SECS: i64 = 10 * 60;

/// How long an alarm waits for its station before it beeps instead
pub const BEEP_AFTER_SECS: i64 = 30;

/// The beep is at least this loud, whatever volume the alarm set, once
/// its fade in is done
pub const BEEP_VOLUME: f32 = 0.5;

/// Gives a station's cached logo as PNG, for a recording's cover art. One
/// is built at start up and shared by every way a recording starts (the
/// window, the schedule, the phone remote and MCP), so they all agree.
/// `None` when the logo isn't cached: a recording doesn't wait for one.
pub type CoverSource = std::sync::Arc<dyn Fn(&Station) -> Option<Vec<u8>> + Send + Sync>;

/// Why an entry whose station never played failed, for its row
const STATION_DID_NOT_START: &str = "the station didn't start";

/// One run of an entry: the entry, and when the run began (Unix seconds)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RunId {
    id: u64,
    at: i64,
}

/// An entry the schedule started that plays now
pub(super) struct ActiveEntry {
    entry: Entry,
    until: Option<DateTime<Local>>,
    started: DateTime<Local>,
    /// The volume to put back when it ends, if it set its own and the user
    /// hasn't moved it since
    restore_volume: Option<f32>,
    /// When to try its station again, after it failed
    retry_at: Option<Instant>,
    /// The station (by `play_seq`) a recording was last started for: a
    /// recording that can't start isn't tried every second
    record_tried: u64,
    /// Its station has played
    heard: bool,
    /// Its station didn't start, and the alarm beeps instead
    beeping: bool,
}

impl ActiveEntry {
    fn run(&self) -> RunId {
        RunId {
            id: self.entry.id,
            at: self.started.timestamp(),
        }
    }
}

/// `text` as the reason after "Last: failed, ": without its capital
fn as_reason(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The sleep timer
pub(super) struct SleepTimer {
    until: DateTime<Local>,
    fade: bool,
    /// The length it was started with
    minutes: u32,
}

/// An alarm's rise from silence
pub(super) enum FadeIn {
    /// Silent until its station plays
    Waiting,
    Rising {
        from: Instant,
    },
}

impl AppController {
    /// Load the schedule from `path` and keep it there
    pub fn with_schedule(mut self, path: PathBuf) -> Self {
        match storage::load_from::<ScheduleFile>(&path) {
            Ok(Some(file)) => {
                self.schedule = file.entries;
                self.schedule_results = file.results;
                // A result whose entry was taken out of the file by hand
                let entries = &self.schedule;
                self.schedule_results
                    .retain(|r| entries.iter().any(|e| e.id == r.id));
            }
            Ok(None) => {}
            Err(e) => eprintln!("Schedule not loaded: {e}"),
        }
        self.schedule_path = Some(path);
        self.schedule_changed();
        self
    }

    /// Where a scheduled recording gets its cover art
    pub fn with_cover_source(mut self, cover: CoverSource) -> Self {
        self.cover_source = Some(cover);
        self
    }

    /// Something to watch the clock for
    pub(super) fn schedule_armed(&self) -> bool {
        self.sleep.is_some() || self.active.is_some() || self.schedule.iter().any(|e| e.enabled)
    }

    /// A fade runs: the volume changes every step
    pub(super) fn fading(&self, now: &DateTime<Local>) -> bool {
        matches!(self.fade_in, Some(FadeIn::Rising { .. }))
            || self.stop_fade_until(now).is_some()
            || self
                .sleep
                .as_ref()
                .is_some_and(|s| s.fade && (s.until - *now).num_seconds() < FADE_OUT_SECS)
    }

    /// Run what is due at `now`, end what is over, and step the fades
    pub(super) fn run_schedule(&mut self, now: DateTime<Local>) {
        let running = Running {
            scheduled_recording: self.scheduled_recording(),
        };
        let (decisions, changed) = schedule::run_due(&mut self.schedule, &now, running);
        let mut notices = Vec::new();
        for decision in &decisions {
            match decision {
                Decision::NotRun { last, notice } => {
                    schedule::keep_result(&mut self.schedule_results, last.clone());
                    notices.extend(notice.clone());
                }
                Decision::Stop { entry } => schedule::keep_result(
                    &mut self.schedule_results,
                    LastRun {
                        id: entry.id,
                        at: now.timestamp(),
                        outcome: Outcome::Stopped,
                    },
                ),
                Decision::Start { .. } => {}
            }
        }
        if changed {
            self.save_schedule();
        }
        for decision in decisions {
            match decision {
                Decision::Start { entry, until } => self.start_entry(*entry, until, &now),
                // A fading stop faded over the minute before it
                Decision::Stop { .. } => self.stop_for_schedule(),
                Decision::NotRun { .. } => {}
            }
        }
        // One notice holds one text: several entries missed at once (the
        // computer slept through them) are counted, and their rows in the
        // Scheduler say which
        match notices.as_slice() {
            [] => {}
            [text] => self.notify(text, NoticeKind::Scheduler),
            many => self.notify(
                &format!("{} scheduled entries didn't run", many.len()),
                NoticeKind::Scheduler,
            ),
        }

        self.look_after_active(&now);
        self.run_sleep_timer(&now);
        self.apply_volume(&now);
        self.publish_schedule(&now);
    }

    /// The window or an agent took over playback: what the schedule started
    /// is theirs now
    pub(super) fn release_schedule(&mut self) {
        if self.active.take().is_some() || self.fade_in.take().is_some() {
            self.apply_volume(&Local::now());
        }
    }

    /// The user set the volume or mute: fades end at their level, and an
    /// entry's volume isn't put back
    pub(super) fn volume_set_by_user(&mut self) {
        self.fade_in = None;
        self.stop_fade_skipped = self.stop_fade_until(&Local::now());
        if let Some(sleep) = self.sleep.as_mut() {
            sleep.fade = false;
        }
        if let Some(active) = self.active.as_mut() {
            active.restore_volume = None;
        }
    }

    /// Playback was stopped by the user: the sleep timer has nothing to do
    pub(super) fn stopped_by_user(&mut self) {
        self.release_schedule();
        self.sleep = None;
    }

    /// The entry's station plays: an alarm's fade can rise
    pub(super) fn entry_station_plays(&mut self) {
        if matches!(self.fade_in, Some(FadeIn::Waiting)) {
            self.fade_in = Some(FadeIn::Rising {
                from: Instant::now(),
            });
        }
    }

    pub(super) fn set_sleep_timer(&mut self, minutes: Option<u32>, fade: bool) {
        let now = Local::now();
        self.sleep = minutes.filter(|m| *m > 0).map(|m| {
            let minutes = m.min(schedule::MAX_MINUTES);
            SleepTimer {
                until: now + TimeDelta::minutes(minutes.into()),
                fade,
                minutes,
            }
        });
        self.apply_volume(&now);
        self.publish_schedule(&now);
    }

    pub(super) fn set_sleep_fade(&mut self, fade: bool) {
        if let Some(sleep) = self.sleep.as_mut() {
            sleep.fade = fade;
        }
        let now = Local::now();
        self.apply_volume(&now);
        self.publish_schedule(&now);
    }

    pub(super) fn save_schedule_entry(&mut self, entry: Entry, reply: ScheduleReply<u64>) {
        let result = self.put_entry(entry);
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    }

    fn put_entry(&mut self, entry: Entry) -> Result<u64, String> {
        let now = Local::now();
        let mut entry = entry.tidied(&now);
        entry.check()?;
        let new = entry.id == 0;
        if new && self.schedule.len() >= schedule::MAX_ENTRIES {
            return Err(format!(
                "The Scheduler is full ({} entries). Remove one to add another.",
                schedule::MAX_ENTRIES
            ));
        }
        if !new && !self.schedule.iter().any(|e| e.id == entry.id) {
            return Err("This entry was removed".into());
        }
        if let Some(other) =
            schedule::clashing_recording(&self.schedule, &entry, now.date_naive(), &Local)
        {
            return Err(clash_message(other));
        }
        // Times already past when it was saved don't run
        entry.armed_from = now.timestamp();
        let id = if new {
            let id = self.schedule.iter().map(|e| e.id).max().unwrap_or(0) + 1;
            entry.id = id;
            self.schedule.push(entry);
            id
        } else {
            let id = entry.id;
            if let Some(slot) = self.schedule.iter_mut().find(|e| e.id == id) {
                *slot = entry;
            }
            id
        };
        self.save_schedule();
        self.publish_schedule(&now);
        Ok(id)
    }

    pub(super) fn remove_schedule_entry(&mut self, id: u64, reply: ScheduleReply<()>) {
        let before = self.schedule.len();
        self.schedule.retain(|e| e.id != id);
        self.schedule_results.retain(|r| r.id != id);
        let result = if self.schedule.len() == before {
            Err("This entry was removed".into())
        } else {
            self.save_schedule();
            self.publish_schedule(&Local::now());
            Ok(())
        };
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    }

    pub(super) fn set_schedule_entry_enabled(
        &mut self,
        id: u64,
        enabled: bool,
        reply: ScheduleReply<()>,
    ) {
        let result = match self.schedule.iter().find(|e| e.id == id).cloned() {
            None => Err("This entry was removed".into()),
            Some(entry) if entry.enabled == enabled => Ok(()),
            Some(entry) if enabled => {
                // A "once" entry that ran comes round again on the next day
                // its time comes
                self.put_entry(Entry {
                    enabled: true,
                    ..entry
                })
                .map(|_| ())
            }
            Some(_) => {
                if let Some(entry) = self.schedule.iter_mut().find(|e| e.id == id) {
                    entry.enabled = false;
                }
                self.save_schedule();
                self.publish_schedule(&Local::now());
                Ok(())
            }
        };
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    }

    fn scheduled_recording(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|a| a.entry.action == Action::Record)
    }

    fn start_entry(&mut self, entry: Entry, until: Option<DateTime<Local>>, now: &DateTime<Local>) {
        let Some(station) = entry.station.clone() else {
            return;
        };
        let (previous_volume, same_station) = {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            let previous = state.volume;
            if let Some(volume) = entry.volume {
                state.volume = volume.clamp(0.0, 1.0);
            }
            // An alarm is heard; a recording is made before the volume
            if entry.action == Action::Play {
                state.is_muted = false;
            }
            let same = state.station_url.as_deref() == Some(station.url.as_str())
                && (state.playback == PlaybackState::Playing || state.is_resolving);
            (previous, same)
        };
        if !same_station {
            self.start_new(
                &station.url,
                Some(station.name.clone()).filter(|n| !n.is_empty()),
                station.logo_url.clone(),
                station.country.clone(),
            );
        }
        self.fade_in = (entry.fade && entry.action == Action::Play).then(|| {
            if same_station {
                FadeIn::Rising {
                    from: Instant::now(),
                }
            } else {
                FadeIn::Waiting
            }
        });
        self.active = Some(ActiveEntry {
            restore_volume: entry.volume.map(|_| previous_volume),
            entry,
            until,
            started: *now,
            retry_at: None,
            record_tried: 0,
            heard: false,
            beeping: false,
        });
    }

    /// Record a recording entry's station once it plays, try a failed
    /// station again, and end the entry when its time is up
    fn look_after_active(&mut self, now: &DateTime<Local>) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if active.until.as_ref().is_some_and(|until| until <= now) {
            self.end_active();
            return;
        }
        // An alarm with no end is the user's after a while
        if active.until.is_none() && (*now - active.started).num_seconds() >= ALARM_WATCH_SECS {
            let beeping = active.beeping;
            let unheard = (!active.heard && !beeping).then(|| active.run());
            self.active = None;
            if let Some(run) = unheard {
                self.keep_run_result(run, failed(STATION_DID_NOT_START));
            }
            if beeping {
                self.stop_playback();
            }
            return;
        }
        // The beep goes on until the user or the entry's end stops it
        if active.beeping {
            return;
        }
        let waited = (*now - active.started).num_seconds();
        let heard = active.heard;
        let fallback = active.entry.fallback;

        let (playback, resolving, play_seq, recording) = {
            let state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            (
                state.playback,
                state.is_resolving,
                state.play_seq,
                state.recording.is_some(),
            )
        };
        let record = active.entry.action == Action::Record;
        let record_tried = active.record_tried;
        let retry_at = active.retry_at;
        let station = active.entry.station.clone();
        let run = active.run();

        if playback == PlaybackState::Playing {
            if let Some(active) = self.active.as_mut() {
                active.retry_at = None;
                active.heard = true;
            }
            if !record && !heard {
                self.keep_run_result(run, Outcome::Played);
            }
            if record && recording && self.recording_for.is_none() {
                // A recording already running when the entry came round
                // is the entry's now
                self.recording_for = Some(run);
            }
            if record && !recording && record_tried != play_seq {
                if let Some(active) = self.active.as_mut() {
                    active.record_tried = play_seq;
                }
                self.record_for_schedule(station.as_ref(), run);
            }
        } else if !record && fallback && !heard && waited >= BEEP_AFTER_SECS {
            // The alarm must wake someone
            self.start_alarm_tone(now);
        } else if playback == PlaybackState::Stopped && !resolving && self.resume.is_none() {
            // The station failed (the engine already tried for a while), and
            // no recording waits for it to start again
            match retry_at {
                None => {
                    if let Some(active) = self.active.as_mut() {
                        active.retry_at = Some(Instant::now() + RETRY_AFTER);
                    }
                }
                Some(at) if Instant::now() >= at => {
                    if let Some(active) = self.active.as_mut() {
                        active.retry_at = None;
                    }
                    if let Some(station) = station {
                        self.start_stream(
                            &station.url,
                            Some(station.name).filter(|n| !n.is_empty()),
                            station.logo_url,
                            station.country,
                        );
                    }
                }
                Some(_) => {}
            }
        }
    }

    /// Beep in place of an alarm's station that hasn't started
    fn start_alarm_tone(&mut self, now: &DateTime<Local>) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.beeping = true;
        let run = active.run();
        // An alarm that fades in lets its beep rise the same way
        let fade = active.entry.fade;
        let name = active
            .entry
            .station
            .as_ref()
            .map(|s| s.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "The station".into());
        self.fade_in = fade.then(|| FadeIn::Rising {
            from: Instant::now(),
        });
        // Stop the station or its resolve; the beep's own events are
        // ignored, as no station is current
        self.cancel_stream();
        self.metadata_rx = None;
        self.stream_failed = false;
        if let Some(engine) = &self.engine {
            engine.stop();
            engine.play(Box::new(AlarmTone::new()), Some("wav".into()), None);
        }
        {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            state.is_resolving = false;
            state.playback = PlaybackState::Playing;
            state.alarm_beep = true;
            state.status_text = format!("{name} didn't start. Playing the fallback sound.").into();
            state.is_error = true;
            state.title.clear();
            state.artist.clear();
            state.codec_name.clear();
        }
        self.keep_run_result(run, failed(STATION_DID_NOT_START));
        self.apply_volume(now);
    }

    /// Record the station of the entry running as `run`. A recording that
    /// can't start is said under the station and in the entry's row.
    fn record_for_schedule(&mut self, station: Option<&ScheduledStation>, run: RunId) {
        let (setup, eq_on) = {
            let state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            (state.recording_setup.clone(), state.eq_enabled)
        };
        let cover = match (&self.cover_source, station) {
            (Some(cover), Some(station)) => cover(&Station::new(&station.name, &station.url)),
            _ => None,
        };
        let started = self.try_start_recording(
            &recordings::folder(setup.dir.as_deref()),
            setup.format,
            setup.bitrate,
            setup.with_eq && eq_on,
            cover,
        );
        match started {
            Ok(()) => self.recording_for = Some(run),
            Err(e) => {
                self.keep_run_result(run, failed(&as_reason(&e)));
                self.notify(&e, NoticeKind::Error);
            }
        }
    }

    /// A recording ended. One made for a scheduled entry is that entry's
    /// last result: how long it is, or why it stopped.
    pub(super) fn scheduled_recording_finished(&mut self, status: &RecordingStatus) {
        let Some(run) = self.recording_for.take() else {
            return;
        };
        let outcome = match &status.error {
            Some(e) => failed(&as_reason(e)),
            None => Outcome::Recorded {
                // To the nearest minute, and never "0 min"
                minutes: ((status.duration.as_secs() + 30) / 60).max(1) as u32,
            },
        };
        self.keep_run_result(run, outcome);
    }

    /// Keep `outcome` as the last result of the entry that ran as `run`,
    /// and save it. What went wrong in a run stays said: a recording
    /// that failed and was started again isn't "recorded".
    fn keep_run_result(&mut self, run: RunId, outcome: Outcome) {
        if !self.schedule.iter().any(|e| e.id == run.id) {
            return;
        }
        let failed_before = self.schedule_results.iter().any(|r| {
            r.id == run.id && r.at == run.at && matches!(r.outcome, Outcome::Failed { .. })
        });
        if failed_before && !matches!(outcome, Outcome::Failed { .. }) {
            return;
        }
        schedule::keep_result(
            &mut self.schedule_results,
            LastRun {
                id: run.id,
                at: run.at,
                outcome,
            },
        );
        self.save_schedule();
    }

    /// The entry's time is up: stop, and put its volume back
    fn end_active(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        self.fade_in = None;
        let run = active.run();
        self.stop_playback();
        // Nothing was heard or recorded in all its time
        let has_result = self
            .schedule_results
            .iter()
            .any(|r| r.id == run.id && r.at == run.at);
        if !has_result {
            self.keep_run_result(run, failed(STATION_DID_NOT_START));
        }
        if let (Some(previous), Some(set)) = (active.restore_volume, active.entry.volume) {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            if (state.volume - set).abs() < 0.001 {
                state.volume = previous;
            }
        }
    }

    /// A bedtime stop: stops whatever plays, as Stop does. The fade that
    /// led to it is over with it, so the next `apply_volume` puts the
    /// user's volume back: for the next station, as the engine keeps a
    /// stopped station at the volume it had while it fades out and ends.
    fn stop_for_schedule(&mut self) {
        self.active = None;
        self.fade_in = None;
        self.stop_playback();
    }

    fn run_sleep_timer(&mut self, now: &DateTime<Local>) {
        let Some(sleep) = self.sleep.as_ref() else {
            return;
        };
        if sleep.until > *now {
            return;
        }
        self.sleep = None;
        if self.scheduled_recording() {
            self.notify(
                "Sleep Timer ended; recording continues",
                NoticeKind::SleepTimer,
            );
            return;
        }
        self.stop_for_schedule();
    }

    /// When a fading Stop due within the minute stops playback: its fade
    /// runs now. Not while a scheduled recording runs (the Stop is skipped
    /// then), nor after the user moved the volume during it.
    fn stop_fade_until(&self, now: &DateTime<Local>) -> Option<DateTime<Local>> {
        if self.scheduled_recording() {
            return None;
        }
        self.schedule
            .iter()
            .filter(|e| e.action == Action::Stop && e.fade)
            .filter_map(|e| e.next_start(now))
            .filter(|at| (*at - *now).num_milliseconds() <= FADE_OUT_SECS * 1000)
            .min()
            .filter(|at| self.stop_fade_skipped != Some(*at))
    }

    /// How loud the fades let the station play now, from 0 to 1
    fn fade_gain(&mut self, now: &DateTime<Local>) -> f32 {
        let mut gain = 1.0;
        match self.fade_in {
            Some(FadeIn::Waiting) => gain = 0.0,
            Some(FadeIn::Rising { from }) => {
                let risen = from.elapsed().as_secs_f32() / FADE_IN.as_secs_f32();
                if risen >= 1.0 {
                    self.fade_in = None;
                } else {
                    gain = risen;
                }
            }
            None => {}
        }
        let fade_outs = [
            self.sleep.as_ref().filter(|s| s.fade).map(|s| s.until),
            self.stop_fade_until(now),
        ];
        for until in fade_outs.into_iter().flatten() {
            let left = (until - *now).num_milliseconds() as f32;
            gain *= (left / (FADE_OUT_SECS * 1000) as f32).clamp(0.0, 1.0);
        }
        gain
    }

    /// Give the engine the volume, with the fades on it
    pub(super) fn apply_volume(&mut self, now: &DateTime<Local>) {
        let gain = self.fade_gain(now);
        let volume = {
            let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
            // Shown on the volume bar, while there is a station to hear
            let sounding = state.playback == PlaybackState::Playing || state.is_resolving;
            state.fade_gain = if sounding && !state.alarm_beep {
                gain
            } else {
                1.0
            };
            if state.is_muted {
                0.0
            } else if state.alarm_beep {
                state.volume.max(BEEP_VOLUME) * gain
            } else {
                state.volume * gain
            }
        };
        if self.engine_volume == Some(volume) {
            return;
        }
        self.engine_volume = Some(volume);
        if let Some(engine) = &self.engine {
            engine.set_volume(volume);
        }
    }

    fn save_schedule(&mut self) {
        self.schedule_changed();
        let Some(path) = &self.schedule_path else {
            return;
        };
        let file = ScheduleFile {
            version: schedule::FILE_VERSION,
            entries: self.schedule.clone(),
            results: self.schedule_results.clone(),
        };
        if let Err(e) = storage::save_to(path, &file) {
            eprintln!("Schedule not saved: {e}");
        }
    }

    /// The entries changed: the window's list needs them
    fn schedule_changed(&mut self) {
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        state.schedule = self.schedule.clone();
        state.schedule_results = self.schedule_results.clone();
        state.schedule_seq += 1;
    }

    /// Mirror the timer, the running entry and the next run into the state
    pub(super) fn publish_schedule(&mut self, now: &DateTime<Local>) {
        let sleep = self.sleep.as_ref().map(|s| SleepTimerInfo {
            until: s.until,
            fade: s.fade,
            minutes: s.minutes,
        });
        let scheduled = self.active.as_ref().map(|a| ScheduledNow {
            id: a.entry.id,
            title: a.entry.title(),
            until: a.until,
            record: a.entry.action == Action::Record,
        });
        let next_run = schedule::next_run(&self.schedule, now).map(|(at, e)| NextRun {
            id: e.id,
            at,
            title: e.title(),
            action: e.action,
            station: e
                .station
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_default(),
        });
        let mut state = self.shared_state.lock().unwrap_or_else(|e| e.into_inner());
        state.sleep = sleep;
        state.scheduled = scheduled;
        state.next_run = next_run;
    }
}

fn failed(reason: &str) -> Outcome {
    Outcome::Failed {
        reason: reason.to_string(),
    }
}

/// "Overlaps Record BBC Radio 3, Saturdays · until 22:00"
pub fn clash_message(other: &Entry) -> String {
    format!(
        "Overlaps {} at {}, {}. Only one station records at a time.",
        other.title(),
        other.start,
        other.details()
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use radiotrope::audio::{AudioEngine, AudioEvent, EngineConfig, EngineOutput};
    use radiotrope_app::data::schedule::{ClockTime, Days, End};

    use super::*;
    use crate::app::state::{AppCommand, AppSnapshot};

    fn controller() -> (AppController, Arc<Mutex<AppSnapshot>>) {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (analysis_tx, _) = crossbeam_channel::unbounded();
        let (stats_tx, _) = crossbeam_channel::unbounded();
        let state = Arc::new(Mutex::new(AppSnapshot::default()));
        let controller = AppController::new(cmd_rx, cmd_tx, state.clone(), analysis_tx, stats_tx);
        (controller, state)
    }

    /// Nothing listens there, so its resolve fails at once
    const DEAD_URL: &str = "http://127.0.0.1:1/jazz.mp3";

    fn playing(state: &Arc<Mutex<AppSnapshot>>) {
        let mut s = state.lock().unwrap();
        s.playback = PlaybackState::Playing;
        s.is_resolving = false;
    }

    /// An entry two minutes from now, every day
    fn entry(action: Action) -> Entry {
        let soon = Local::now() + TimeDelta::minutes(2);
        Entry {
            id: 0,
            enabled: true,
            action,
            station: (action != Action::Stop).then(|| ScheduledStation {
                name: "Jazz FM".into(),
                url: DEAD_URL.into(),
                ..Default::default()
            }),
            start: ClockTime::parse(&soon.format("%H:%M").to_string()).unwrap(),
            days: Days::EVERY_DAY,
            date: None,
            end: End::Never,
            volume: None,
            fade: false,
            fallback: true,
            armed_from: 0,
        }
    }

    /// Save `entry` and say when it comes round
    fn save(controller: &mut AppController, entry: Entry) -> (u64, DateTime<Local>) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        controller.handle_command(AppCommand::SaveScheduleEntry {
            entry,
            reply: Some(reply),
        });
        let id = answer.blocking_recv().unwrap().unwrap();
        let saved = controller.schedule.iter().find(|e| e.id == id).unwrap();
        (id, saved.next_start(&Local::now()).unwrap())
    }

    #[test]
    fn the_sleep_timer_fades_out_and_stops() {
        let (mut controller, state) = controller();
        state.lock().unwrap().volume = 0.8;
        playing(&state);
        controller.handle_command(AppCommand::SetSleepTimer {
            minutes: Some(10),
            fade: true,
        });
        let until = state.lock().unwrap().sleep.clone().unwrap().until;

        controller.run_schedule(until - TimeDelta::minutes(5));
        assert_eq!(controller.engine_volume, Some(0.8));
        // Half way through the last minute: half as loud
        controller.run_schedule(until - TimeDelta::seconds(30));
        let volume = controller.engine_volume.unwrap();
        assert!((volume - 0.4).abs() < 0.02, "{volume}");
        assert!(controller.fading(&(until - TimeDelta::seconds(30))));

        controller.run_schedule(until);
        let s = state.lock().unwrap();
        assert_eq!(s.playback, PlaybackState::Stopped);
        assert!(s.sleep.is_none());
        // The volume setting is untouched, and the engine back at it
        assert_eq!(s.volume, 0.8);
        drop(s);
        assert_eq!(controller.engine_volume, Some(0.8));
    }

    #[test]
    fn the_sleep_timers_fade_goes_down_evenly_over_the_last_minute() {
        // The fade follows the clock it is given, not the wall clock: every
        // point of its curve is checked at an exact time
        let (mut controller, state) = controller();
        state.lock().unwrap().volume = 0.8;
        playing(&state);
        controller.handle_command(AppCommand::SetSleepTimer {
            minutes: Some(10),
            fade: true,
        });
        let until = state.lock().unwrap().sleep.clone().unwrap().until;

        let mut last = f32::MAX;
        for left in (1..=FADE_OUT_SECS + 5).rev() {
            controller.apply_volume(&(until - TimeDelta::seconds(left)));
            let volume = controller.engine_volume.unwrap();
            let gain = state.lock().unwrap().fade_gain;
            let expected = (left as f32 / FADE_OUT_SECS as f32).min(1.0);
            assert!((gain - expected).abs() < 1e-4, "{left} s left: {gain}");
            assert!((volume - 0.8 * expected).abs() < 1e-4, "{left} s left");
            assert!(volume <= last, "louder at {left} s left");
            last = volume;
        }
        // Still playing until the end, and stopped there
        assert_eq!(state.lock().unwrap().playback, PlaybackState::Playing);
        controller.run_schedule(until);
        assert_eq!(state.lock().unwrap().playback, PlaybackState::Stopped);
        assert_eq!(controller.engine_volume, Some(0.8));
    }

    #[test]
    fn moving_the_volume_ends_the_fade_but_not_the_timer() {
        let (mut controller, state) = controller();
        playing(&state);
        controller.handle_command(AppCommand::SetSleepTimer {
            minutes: Some(1),
            fade: true,
        });
        let until = state.lock().unwrap().sleep.clone().unwrap().until;
        controller.run_schedule(until - TimeDelta::seconds(15));
        controller.handle_command(AppCommand::SetVolume(0.5));
        assert_eq!(controller.engine_volume, Some(0.5));
        controller.run_schedule(until - TimeDelta::seconds(5));
        assert_eq!(controller.engine_volume, Some(0.5));
        controller.run_schedule(until);
        assert_eq!(state.lock().unwrap().playback, PlaybackState::Stopped);
    }

    #[test]
    fn stopping_by_hand_ends_the_sleep_timer() {
        let (mut controller, state) = controller();
        playing(&state);
        controller.handle_command(AppCommand::SetSleepTimer {
            minutes: Some(30),
            fade: true,
        });
        assert!(state.lock().unwrap().sleep.is_some());
        controller.handle_command(AppCommand::Stop);
        controller.run_schedule(Local::now());
        assert!(state.lock().unwrap().sleep.is_none());
        controller.handle_command(AppCommand::SetSleepTimer {
            minutes: Some(30),
            fade: true,
        });
        controller.handle_command(AppCommand::SetSleepTimer {
            minutes: None,
            fade: true,
        });
        assert!(state.lock().unwrap().sleep.is_none());
    }

    #[test]
    fn an_alarm_whose_station_does_not_start_beeps_until_stopped() {
        let (mut controller, state) = controller();
        state.lock().unwrap().volume = 0.2;
        let (_, at) = save(&mut controller, entry(Action::Play));
        controller.run_schedule(at);
        controller.run_schedule(at + TimeDelta::seconds(BEEP_AFTER_SECS - 1));
        assert!(!state.lock().unwrap().alarm_beep);

        controller.run_schedule(at + TimeDelta::seconds(BEEP_AFTER_SECS));
        {
            let s = state.lock().unwrap();
            assert!(s.alarm_beep);
            assert_eq!(s.playback, PlaybackState::Playing);
            assert!(s.status_text.contains("Jazz FM didn't start"));
        }
        // Loud enough to hear
        assert_eq!(controller.engine_volume, Some(BEEP_VOLUME));

        // It isn't retried over the beep, and Stop silences it
        controller.run_schedule(at + TimeDelta::seconds(BEEP_AFTER_SECS + 40));
        assert!(state.lock().unwrap().alarm_beep);
        controller.handle_command(AppCommand::Stop);
        let s = state.lock().unwrap();
        assert!(!s.alarm_beep);
        assert_eq!(s.playback, PlaybackState::Stopped);
        drop(s);
        assert_eq!(controller.engine_volume, Some(0.2));
    }

    #[test]
    fn a_fading_alarms_beep_rises_like_its_station_would() {
        let (mut controller, state) = controller();
        state.lock().unwrap().volume = 0.2;
        let (_, at) = save(
            &mut controller,
            Entry {
                fade: true,
                ..entry(Action::Play)
            },
        );
        controller.run_schedule(at);
        let beep_at = at + TimeDelta::seconds(BEEP_AFTER_SECS);
        controller.run_schedule(beep_at);
        assert!(state.lock().unwrap().alarm_beep);
        // It starts from silence
        assert!(controller.engine_volume.unwrap() < 0.05);

        // and ends at the beep's volume
        controller.fade_in = Some(FadeIn::Rising {
            from: Instant::now() - FADE_IN,
        });
        controller.apply_volume(&beep_at);
        assert_eq!(controller.engine_volume, Some(BEEP_VOLUME));
    }

    #[test]
    fn an_alarm_without_the_fallback_sound_stays_quiet() {
        let (mut controller, state) = controller();
        let (_, at) = save(
            &mut controller,
            Entry {
                fallback: false,
                ..entry(Action::Play)
            },
        );
        controller.run_schedule(at);
        controller.run_schedule(at + TimeDelta::seconds(BEEP_AFTER_SECS + 5));
        assert!(!state.lock().unwrap().alarm_beep);
    }

    #[test]
    fn the_timer_keeps_its_length_for_restart() {
        let (mut controller, state) = controller();
        controller.handle_command(AppCommand::SetSleepTimer {
            minutes: Some(45),
            fade: true,
        });
        assert_eq!(state.lock().unwrap().sleep.as_ref().unwrap().minutes, 45);
    }

    #[test]
    fn the_beep_ends_with_the_alarm_watch_and_a_heard_station_never_beeps() {
        let (mut controller, state) = controller();
        let (_, at) = save(&mut controller, entry(Action::Play));
        controller.run_schedule(at);
        controller.run_schedule(at + TimeDelta::seconds(BEEP_AFTER_SECS));
        assert!(state.lock().unwrap().alarm_beep);
        controller.run_schedule(at + TimeDelta::seconds(ALARM_WATCH_SECS));
        assert!(!state.lock().unwrap().alarm_beep);
        assert_eq!(state.lock().unwrap().playback, PlaybackState::Stopped);
        assert!(controller.active.is_none());

        // Heard, then dropped out: the retry, not a beep
        let (mut controller, state) = self::controller();
        let (_, at) = save(&mut controller, entry(Action::Play));
        controller.run_schedule(at);
        playing(&state);
        controller.run_schedule(at + TimeDelta::seconds(5));
        state.lock().unwrap().playback = PlaybackState::Stopped;
        controller.run_schedule(at + TimeDelta::seconds(BEEP_AFTER_SECS + 5));
        assert!(!state.lock().unwrap().alarm_beep);

        // A recording doesn't beep
        let (mut controller, state) = self::controller();
        let (_, at) = save(
            &mut controller,
            Entry {
                end: End::After { minutes: 30 },
                ..entry(Action::Record)
            },
        );
        controller.run_schedule(at);
        controller.run_schedule(at + TimeDelta::seconds(BEEP_AFTER_SECS + 5));
        assert!(!state.lock().unwrap().alarm_beep);
    }

    #[test]
    fn an_alarm_plays_its_station_at_its_volume_and_rises() {
        let (mut controller, state) = controller();
        {
            let mut s = state.lock().unwrap();
            s.volume = 0.8;
            s.is_muted = true;
        }
        let (id, at) = save(
            &mut controller,
            Entry {
                volume: Some(0.3),
                fade: true,
                end: End::After { minutes: 60 },
                ..entry(Action::Play)
            },
        );
        assert_eq!(state.lock().unwrap().next_run.as_ref().unwrap().id, id);

        controller.run_schedule(at - TimeDelta::seconds(1));
        assert!(controller.active.is_none());

        controller.run_schedule(at);
        {
            let s = state.lock().unwrap();
            assert_eq!(s.station_url.as_deref(), Some(DEAD_URL));
            assert_eq!(s.volume, 0.3);
            assert!(!s.is_muted, "an alarm is heard");
            assert_eq!(s.scheduled.as_ref().unwrap().title, "Play Jazz FM");
        }
        // Silent until the station plays, then rising
        assert_eq!(controller.engine_volume, Some(0.0));
        playing(&state);
        controller.entry_station_plays();
        controller.run_schedule(at + TimeDelta::seconds(1));
        let volume = controller.engine_volume.unwrap();
        assert!(volume < 0.05, "{volume}");

        // Over after an hour: stopped, and the volume put back
        controller.run_schedule(at + TimeDelta::minutes(60));
        let s = state.lock().unwrap();
        assert_eq!(s.playback, PlaybackState::Stopped);
        assert_eq!(s.volume, 0.8);
        assert!(s.scheduled.is_none());
    }

    #[test]
    fn a_station_picked_by_hand_isnt_stopped_by_the_alarms_end() {
        let (mut controller, state) = controller();
        let (_, at) = save(
            &mut controller,
            Entry {
                volume: Some(0.3),
                end: End::After { minutes: 30 },
                ..entry(Action::Play)
            },
        );
        controller.run_schedule(at);
        assert!(controller.active.is_some());
        controller.handle_command(AppCommand::Play {
            url: "http://127.0.0.1:1/other.mp3".into(),
            backups: Vec::new(),
            name: Some("Other".into()),
            logo_url: None,
            country: None,
            taken: None,
        });
        assert!(controller.active.is_none());
        playing(&state);
        controller.run_schedule(at + TimeDelta::minutes(30));
        let s = state.lock().unwrap();
        assert_eq!(s.playback, PlaybackState::Playing);
        assert_eq!(s.volume, 0.3, "the alarm's volume is the user's now");
    }

    #[test]
    fn a_failed_station_is_tried_again() {
        let (mut controller, state) = controller();
        let (_, at) = save(&mut controller, entry(Action::Play));
        controller.run_schedule(at);
        let first = state.lock().unwrap().play_seq;
        // It failed
        {
            let mut s = state.lock().unwrap();
            s.is_resolving = false;
            s.playback = PlaybackState::Stopped;
        }
        controller.run_schedule(at + TimeDelta::seconds(1));
        assert_eq!(state.lock().unwrap().play_seq, first);
        controller.active.as_mut().unwrap().retry_at = Some(Instant::now());
        controller.run_schedule(at + TimeDelta::seconds(BEEP_AFTER_SECS - 1));
        assert_eq!(state.lock().unwrap().play_seq, first + 1);
    }

    // Report Play Count: a scheduled start is counted once when it plays,
    // and its retries aren't counted again
    #[test]
    fn a_scheduled_start_is_reported_once() {
        let (controller, state) = controller();
        let reports = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut controller = controller.with_play_reporter({
            let reports = reports.clone();
            Arc::new(move |url: &str| reports.lock().unwrap().push(url.to_string()))
        });
        let codec = || radiotrope::audio::CodecInfo {
            codec_name: "MP3".into(),
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: None,
            bitrate: None,
        };
        let plays = |controller: &mut AppController| {
            state.lock().unwrap().is_resolving = false;
            controller.handle_engine_event(radiotrope::audio::EngineEvent {
                stream: None,
                event: AudioEvent::Playing(codec()),
            });
        };
        let (_, at) = save(&mut controller, entry(Action::Play));
        controller.run_schedule(at);
        // It failed, and is tried again
        {
            let mut s = state.lock().unwrap();
            s.is_resolving = false;
            s.playback = PlaybackState::Stopped;
        }
        controller.run_schedule(at + TimeDelta::seconds(1));
        controller.active.as_mut().unwrap().retry_at = Some(Instant::now());
        controller.run_schedule(at + TimeDelta::seconds(2));
        assert!(reports.lock().unwrap().is_empty(), "not before it plays");
        plays(&mut controller);
        assert_eq!(*reports.lock().unwrap(), [DEAD_URL]);
        // It drops out and plays again: the same play
        plays(&mut controller);
        assert_eq!(reports.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_bedtime_stop_stops_any_station() {
        let (mut controller, state) = controller();
        playing(&state);
        let (_, at) = save(&mut controller, entry(Action::Stop));
        controller.run_schedule(at);
        assert_eq!(state.lock().unwrap().playback, PlaybackState::Stopped);
    }

    #[test]
    fn a_fading_stop_fades_over_the_minute_before_it() {
        let (mut controller, state) = controller();
        state.lock().unwrap().volume = 0.8;
        playing(&state);
        controller.set_sleep_timer(Some(90), true);
        let (_, at) = save(
            &mut controller,
            Entry {
                fade: true,
                ..entry(Action::Stop)
            },
        );

        // Full volume until a minute before, half way down at 30 s
        controller.apply_volume(&(at - TimeDelta::seconds(61)));
        assert_eq!(controller.engine_volume, Some(0.8));
        let half = at - TimeDelta::seconds(30);
        assert!(controller.fading(&half));
        controller.apply_volume(&half);
        let volume = controller.engine_volume.unwrap();
        assert!((volume - 0.4).abs() < 0.02, "{volume}");
        // It isn't the Sleep Timer: the one set by hand runs on, unchanged
        assert_eq!(controller.sleep.as_ref().map(|s| s.minutes), Some(90));

        // Stopped on time, the volume setting untouched
        controller.run_schedule(at);
        let s = state.lock().unwrap();
        assert_eq!(s.playback, PlaybackState::Stopped);
        assert_eq!(s.volume, 0.8);
        drop(s);
        assert_eq!(controller.engine_volume, Some(0.8));
    }

    /// A tone that never ends, as a WAV stream
    struct Tone {
        pos: u64,
    }

    impl Tone {
        /// 44.1 kHz, mono, 16-bit, of no known length
        fn header() -> Vec<u8> {
            let mut header = b"RIFF\xff\xff\xff\xffWAVEfmt ".to_vec();
            header.extend_from_slice(&16u32.to_le_bytes());
            header.extend_from_slice(&1u16.to_le_bytes());
            header.extend_from_slice(&1u16.to_le_bytes());
            header.extend_from_slice(&44_100u32.to_le_bytes());
            header.extend_from_slice(&88_200u32.to_le_bytes());
            header.extend_from_slice(&2u16.to_le_bytes());
            header.extend_from_slice(&16u16.to_le_bytes());
            header.extend_from_slice(b"data\xbf\xff\xff\xff");
            header
        }
    }

    impl std::io::Read for Tone {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let header = Self::header();
            for (i, byte) in buf.iter_mut().enumerate() {
                let pos = self.pos + i as u64;
                *byte = match header.get(pos as usize) {
                    Some(b) => *b,
                    None => {
                        let at = pos - header.len() as u64;
                        let sample = ((at / 2) as f32 * 0.1).sin() * 10_000.0;
                        (sample as i16).to_le_bytes()[(at % 2) as usize]
                    }
                };
            }
            self.pos += buf.len() as u64;
            Ok(buf.len())
        }
    }

    impl std::io::Seek for Tone {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.pos = match pos {
                std::io::SeekFrom::Start(n) => n,
                std::io::SeekFrom::Current(n) => self.pos.checked_add_signed(n).unwrap_or(0),
                std::io::SeekFrom::End(_) => return Err(std::io::Error::other("endless")),
            };
            Ok(self.pos)
        }
    }

    /// Play the tone on the controller's engine, and wait until it is heard
    fn play_tone(controller: &AppController, state: &Arc<Mutex<AppSnapshot>>) {
        let engine = controller.engine.as_ref().unwrap();
        engine.play(Box::new(Tone { pos: 0 }), Some("wav".into()), None);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match engine.event_receiver().recv_timeout(left) {
                Ok(e) if matches!(e.event, AudioEvent::Playing(_)) => break,
                Ok(_) => {}
                Err(_) => panic!("the tone doesn't play"),
            }
        }
        playing(state);
        // Past the station's fade in
        std::thread::sleep(std::time::Duration::from_millis(150));
    }

    /// A controller at volume 0.8 whose engine plays the tone into an
    /// output with no device, 20 times as fast as it would be heard.
    ///
    /// Volume matching is off: it starts a new station turned down and
    /// raises it slowly as it learns its loudness, so with it the tone would
    /// keep getting louder. How much louder by a given moment depends on how
    /// much audio a busy machine has played by then, and a reading taken
    /// before the stop could then be beaten by the audio played just before
    /// it, with no blip. Without it the tone plays at one level for each
    /// volume, and any sample louder than the fade's is a real blip.
    fn heard() -> (AppController, Arc<Mutex<AppSnapshot>>) {
        let (mut controller, state) = controller();
        controller.engine = Some(
            AudioEngine::with_config(EngineConfig {
                output: EngineOutput::Silent { speed: 20.0 },
                ..EngineConfig::default()
            })
            .expect("the engine starts"),
        );
        controller.handle_command(AppCommand::SetMatchVolume(false));
        state.lock().unwrap().volume = 0.8;
        controller.apply_volume(&Local::now());
        play_tone(&controller, &state);
        (controller, state)
    }

    /// The loudest sample the engine's output takes in the next `ms`
    fn loudest(controller: &AppController, ms: u64) -> f32 {
        let engine = controller.engine.as_ref().unwrap();
        engine.take_output_peak();
        std::thread::sleep(std::time::Duration::from_millis(ms));
        engine.take_output_peak()
    }

    /// How loud the output is once its volume has settled: read until two
    /// readings in a row agree, as a busy machine may still be fading the
    /// station in, ramping the volume, or catching up on its output, at the
    /// first one. The tone's level only changes with those, so two readings
    /// that agree are its level.
    fn steady(controller: &AppController) -> f32 {
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let mut last = loudest(controller, 100);
        loop {
            let peak = loudest(controller, 100);
            let settled = last > 0.0 && (peak - last).abs() <= last * 0.005;
            if settled || Instant::now() >= deadline {
                return peak.max(last);
            }
            last = peak;
        }
    }

    /// The fade to `stop_at` that `controller` runs ends there with no
    /// sample louder than the fade had got, and the next station plays at
    /// the user's volume
    fn fades_to_a_stop_without_a_blip(
        mut controller: AppController,
        state: &Arc<Mutex<AppSnapshot>>,
        stop_at: DateTime<Local>,
    ) {
        // Past the station's own fade in, however long a busy machine takes
        let full = steady(&controller);
        assert!(full > 0.05, "the station isn't heard: {full}");

        // Six seconds before the end: a tenth as loud
        controller.run_schedule(stop_at - TimeDelta::seconds(6));
        let volume = controller.engine_volume.unwrap();
        assert!((volume - 0.08).abs() < 0.005, "{volume}");
        assert!((state.lock().unwrap().fade_gain - 0.1).abs() < 0.01);
        let faded = steady(&controller);
        assert!(faded > 0.0 && faded < full * 0.05, "not faded: {faded}");

        // The end: stopped, and the volume put back in the same step
        controller.engine.as_ref().unwrap().take_output_peak();
        controller.run_schedule(stop_at);
        assert_eq!(controller.engine_volume, Some(0.8));
        {
            let s = state.lock().unwrap();
            assert_eq!(s.playback, PlaybackState::Stopped);
            // What the window, the tray and the phone show is the user's
            assert_eq!(s.volume, 0.8);
            assert_eq!(s.fade_gain, 1.0);
        }
        // Whatever was still to play (the engine's own short fade-out,
        // what its player had queued) plays out: wait until a reading hears
        // nothing, keeping the loudest sample on the way. A busy machine may
        // take far longer than the fade's 400 ms (20 ms here) to play it.
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let mut after = 0.0f32;
        loop {
            // Not `loudest`: that would drop what played since the stop
            std::thread::sleep(std::time::Duration::from_millis(100));
            let peak = controller.engine.as_ref().unwrap().take_output_peak();
            if peak == 0.0 {
                break;
            }
            after = after.max(peak);
            assert!(Instant::now() < deadline, "still heard: {peak}");
        }
        assert!(
            after <= faded * 1.01,
            "louder after the stop ({after}) than the fade ended at ({faded})"
        );

        // The next station plays at the user's volume, not the fade's: a
        // hundred times louder than the fade ended at, once it has
        // started (a busy machine takes its time)
        play_tone(&controller, state);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let mut next = loudest(&controller, 100);
        while next < faded * 30.0 && Instant::now() < deadline {
            next = loudest(&controller, 100);
        }
        assert!(next >= faded * 30.0, "{next} after a fade to {faded}");
    }

    #[test]
    fn the_sleep_timers_fade_ends_without_a_blip_at_the_users_volume() {
        let (mut controller, state) = heard();
        controller.handle_command(AppCommand::SetSleepTimer {
            minutes: Some(10),
            fade: true,
        });
        let until = state.lock().unwrap().sleep.clone().unwrap().until;
        fades_to_a_stop_without_a_blip(controller, &state, until);
    }

    #[test]
    fn a_fading_stops_fade_ends_without_a_blip_at_the_users_volume() {
        let (mut controller, state) = heard();
        let (_, at) = save(
            &mut controller,
            Entry {
                fade: true,
                ..entry(Action::Stop)
            },
        );
        fades_to_a_stop_without_a_blip(controller, &state, at);
    }

    #[test]
    fn without_smooth_switching_a_fade_ends_without_a_blip_too() {
        let (mut controller, state) = heard();
        controller.handle_command(AppCommand::SetSmoothSwitching(false));
        controller.handle_command(AppCommand::SetSleepTimer {
            minutes: Some(10),
            fade: true,
        });
        let until = state.lock().unwrap().sleep.clone().unwrap().until;
        fades_to_a_stop_without_a_blip(controller, &state, until);
    }

    #[test]
    fn moving_the_volume_ends_a_stops_fade() {
        let (mut controller, state) = controller();
        state.lock().unwrap().volume = 0.8;
        playing(&state);
        let (_, at) = save(
            &mut controller,
            Entry {
                fade: true,
                ..entry(Action::Stop)
            },
        );
        controller.stop_fade_skipped = Some(at);
        controller.apply_volume(&(at - TimeDelta::seconds(30)));
        assert_eq!(controller.engine_volume, Some(0.8));
    }

    /// An alarm set two minutes ahead, then the clock at 20 minutes past
    /// it: the computer slept through it
    fn missed_alarm(controller: &mut AppController) -> u64 {
        let (id, at) = save(controller, entry(Action::Play));
        controller.run_schedule(at + TimeDelta::minutes(20));
        id
    }

    #[test]
    fn a_missed_entry_is_a_scheduler_notice_that_stays() {
        let (mut controller, state) = controller();
        let id = missed_alarm(&mut controller);
        let s = state.lock().unwrap();
        let notice = s.recording_notice.as_ref().expect("a notice");
        assert!(notice.text.starts_with("Missed "), "{}", notice.text);
        assert!(notice.text.ends_with(": Play Jazz FM"), "{}", notice.text);
        assert_eq!(notice.kind, NoticeKind::Scheduler);
        assert!(notice.kind.stays());
        // Not a failure for an agent that asks
        assert!(!notice.is_error);
        // And its row says so
        assert_eq!(s.schedule_results.len(), 1);
        assert_eq!(s.schedule_results[0].id, id);
        assert_eq!(s.schedule_results[0].outcome, Outcome::Missed);
    }

    #[test]
    fn several_entries_missed_at_once_are_counted_in_one_notice() {
        let (mut controller, state) = controller();
        let (_, at) = save(&mut controller, entry(Action::Play));
        save(
            &mut controller,
            Entry {
                station: Some(ScheduledStation {
                    name: "BBC Radio 3".into(),
                    url: DEAD_URL.into(),
                    ..Default::default()
                }),
                ..entry(Action::Play)
            },
        );
        controller.run_schedule(at + TimeDelta::minutes(20));
        let s = state.lock().unwrap();
        let notice = s.recording_notice.as_ref().expect("a notice");
        assert_eq!(notice.text, "2 scheduled entries didn't run");
        assert_eq!(notice.kind, NoticeKind::Scheduler);
        assert_eq!(s.schedule_results.len(), 2);
    }

    #[test]
    fn notices_have_the_kind_of_what_they_are_about() {
        let (mut controller, state) = controller();
        let notice = |state: &Arc<Mutex<AppSnapshot>>| {
            let n = state.lock().unwrap().recording_notice.clone().unwrap();
            (n.kind, n.is_error, n.kind.stays())
        };
        let status = |error: Option<&str>| RecordingStatus {
            path: PathBuf::from("Jazz FM - 2026-10-02 19-30-00.mp3"),
            bytes_written: 1,
            duration: Duration::from_secs(45 * 60),
            dropped_batches: 0,
            error: error.map(String::from),
        };
        // Saved: passes, and opens the folder
        controller.report_finished(&status(None));
        assert_eq!(notice(&state), (NoticeKind::Saved, false, false));
        // Failed: stays
        controller.report_finished(&status(Some("no space left on device")));
        assert_eq!(notice(&state), (NoticeKind::Error, true, true));
        let text = state.lock().unwrap().recording_notice.clone().unwrap().text;
        assert!(
            text.starts_with("Recording stopped: no space left on device. Saved"),
            "{text}"
        );
        // The Sleep Timer's news passes
        controller.notify(
            "Sleep Timer ended; recording continues",
            NoticeKind::SleepTimer,
        );
        assert_eq!(notice(&state), (NoticeKind::SleepTimer, false, false));
        // Each is a new notice for the window
        assert_eq!(
            state.lock().unwrap().recording_notice.as_ref().unwrap().seq,
            3
        );
    }

    #[test]
    fn a_scheduled_recording_leaves_its_length_or_why_it_failed() {
        let (mut controller, state) = controller();
        let (id, at) = save(
            &mut controller,
            Entry {
                end: End::After { minutes: 60 },
                ..entry(Action::Record)
            },
        );
        let status = |secs, error: Option<&str>| RecordingStatus {
            path: PathBuf::from("Jazz FM.mp3"),
            bytes_written: 1,
            duration: Duration::from_secs(secs),
            dropped_batches: 0,
            error: error.map(String::from),
        };
        let run = RunId {
            id,
            at: at.timestamp(),
        };
        let last = |state: &Arc<Mutex<AppSnapshot>>| {
            state.lock().unwrap().schedule_results[0].outcome.clone()
        };

        controller.recording_for = Some(run);
        controller.report_finished(&status(44 * 60 + 40, None));
        assert_eq!(last(&state), Outcome::Recorded { minutes: 45 });
        // The recording is nobody's once it ended
        assert_eq!(controller.recording_for, None);

        // The next evening the disk fills up; a recording started again
        // in the same run doesn't cover that up
        let run = RunId {
            at: run.at + 86_400,
            ..run
        };
        controller.recording_for = Some(run);
        controller.report_finished(&status(600, Some("No space left on device")));
        let failed = Outcome::Failed {
            reason: "no space left on device".into(),
        };
        assert_eq!(last(&state), failed);
        controller.recording_for = Some(run);
        controller.report_finished(&status(600, None));
        assert_eq!(last(&state), failed);

        // A recording made by hand says nothing about the entry
        controller.report_finished(&status(60, None));
        assert_eq!(last(&state), failed);
    }

    #[test]
    fn the_last_result_survives_a_restart_and_goes_with_its_entry() {
        let dir = std::env::temp_dir().join(format!("rt-schedule-last-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(schedule::FILE_NAME);
        let (controller_a, _) = controller();
        let mut controller_a = controller_a.with_schedule(path.clone());
        let id = missed_alarm(&mut controller_a);

        // Started again: the row still says it was missed
        let (controller_b, state_b) = controller();
        let mut controller_b = controller_b.with_schedule(path.clone());
        assert_eq!(controller_b.schedule_results, controller_a.schedule_results);
        {
            let s = state_b.lock().unwrap();
            assert_eq!(s.schedule_results.len(), 1);
            assert_eq!(s.schedule_results[0].id, id);
            assert!(s.schedule_results[0]
                .label(&Local::now())
                .starts_with("Last: missed · "));
        }

        // Changing the entry keeps what its last run did
        let entry = controller_b.schedule[0].clone();
        save(
            &mut controller_b,
            Entry {
                fade: true,
                ..entry
            },
        );
        assert_eq!(controller_b.schedule_results.len(), 1);

        // Removed with its entry, in the file too
        let (reply, answer) = tokio::sync::oneshot::channel();
        controller_b.handle_command(AppCommand::RemoveScheduleEntry {
            id,
            reply: Some(reply),
        });
        answer.blocking_recv().unwrap().unwrap();
        assert!(state_b.lock().unwrap().schedule_results.is_empty());
        let (controller_c, _) = controller();
        assert!(controller_c.with_schedule(path).schedule_results.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_recording_at_the_same_time_is_refused() {
        let (mut controller, _) = controller();
        let show = Entry {
            end: End::After { minutes: 60 },
            ..entry(Action::Record)
        };
        save(&mut controller, show.clone());
        let (reply, answer) = tokio::sync::oneshot::channel();
        controller.handle_command(AppCommand::SaveScheduleEntry {
            entry: show,
            reply: Some(reply),
        });
        let error = answer.blocking_recv().unwrap().unwrap_err();
        assert!(error.starts_with("Overlaps Record Jazz FM"), "{error}");
        assert_eq!(controller.schedule.len(), 1);
    }

    #[test]
    fn entries_are_saved_and_loaded() {
        let dir = std::env::temp_dir().join(format!("rt-schedule-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(schedule::FILE_NAME);
        let (controller_a, _) = controller();
        let mut controller_a = controller_a.with_schedule(path.clone());
        let (id, _) = save(&mut controller_a, entry(Action::Play));
        let (reply, answer) = tokio::sync::oneshot::channel();
        controller_a.handle_command(AppCommand::SetScheduleEntryEnabled {
            id,
            enabled: false,
            reply: Some(reply),
        });
        answer.blocking_recv().unwrap().unwrap();

        let (controller_b, state_b) = controller();
        let controller_b = controller_b.with_schedule(path);
        assert_eq!(controller_b.schedule, controller_a.schedule);
        assert!(!state_b.lock().unwrap().schedule[0].enabled);

        let (reply, answer) = tokio::sync::oneshot::channel();
        controller_a.handle_command(AppCommand::RemoveScheduleEntry {
            id,
            reply: Some(reply),
        });
        answer.blocking_recv().unwrap().unwrap();
        assert!(controller_a.schedule.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
