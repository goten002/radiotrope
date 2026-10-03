//! The window's side of the schedule: the Timer and Scheduler dialogs,
//! and the labels under the station name
//!
//! The controller keeps the entries and runs them; this sends it the
//! changes and shows what it has. A draft is checked here first, with the
//! same rules, so the dialog can say at once what is wrong with it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeDelta, Timelike};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use radiotrope_app::data::schedule::{self, Action, ClockTime, Days, End, Entry, ScheduledStation};

use crate::app::controller::clash_message;
use crate::app::state::{AppCommand, AppSnapshot, NextRun, ScheduledNow, SleepTimerInfo};
use crate::app::ui_sender::UiSender;
use crate::{App, ScheduleDraft, ScheduleRow};
use radiotrope_app::config::ui::{SCHEDULE_COUNTDOWN_SECS, SCHEDULE_DATES, SCHEDULE_LABEL_SECS};

/// Volumes the dialog offers after "Keep current" and "Silent", in percent
const VOLUMES: [u32; 10] = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];

/// Length of a new entry's play or recording, in minutes
const DEFAULT_MINUTES: u32 = 60;

thread_local! {
    /// The stations the open dialog offers, in its list's order
    static STATIONS: RefCell<Vec<ScheduledStation>> = const { RefCell::new(Vec::new()) };
    /// The schedule shown, by `schedule_seq`
    static SHOWN_SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
    /// The entry the label last counted down to, to say when it started
    static COUNTED_DOWN: RefCell<Option<NextRun>> = const { RefCell::new(None) };
    /// The label saying what started was clicked away
    static STARTED_DISMISSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The fallback sound label was clicked away (until the beep ends)
    static BEEP_DISMISSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What the Scheduler's label under the station says
#[derive(Debug, Default, PartialEq)]
struct Label {
    text: String,
    /// For the fallback sound: a bell instead of the calendar
    bell: bool,
    /// Only news about what happened, so a click closes it
    closable: bool,
}

pub fn setup(
    ui: &App,
    settings: &radiotrope_app::data::settings::Settings,
    cmd_tx: UiSender,
    state: Arc<Mutex<AppSnapshot>>,
) {
    ui.set_sleep_fade(settings.sleep_fade);
    let cmd_tx = Rc::new(cmd_tx);

    ui.on_sleep_start({
        let ui_weak = ui.as_weak();
        let cmd_tx = cmd_tx.clone();
        move |minutes| {
            let Some(ui) = ui_weak.upgrade() else { return };
            cmd_tx.send(AppCommand::SetSleepTimer {
                minutes: Some(minutes.max(1) as u32),
                fade: ui.get_sleep_fade(),
            });
        }
    });
    ui.on_sleep_off({
        let cmd_tx = cmd_tx.clone();
        move || {
            cmd_tx.send(AppCommand::SetSleepTimer {
                minutes: None,
                fade: true,
            })
        }
    });
    ui.on_sleep_fade_toggled({
        let cmd_tx = cmd_tx.clone();
        move |on| {
            cmd_tx.send(AppCommand::SetSleepFade(on));
            let mut settings = radiotrope_app::data::settings::Settings::load().unwrap_or_default();
            settings.sleep_fade = on;
            if let Err(e) = settings.save() {
                eprintln!("Failed to save the sleep timer setting: {e}");
            }
        }
    });

    ui.on_schedule_label_dismiss({
        let ui_weak = ui.as_weak();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            dismiss_label(ui.get_schedule_label_bell());
            ui.set_schedule_label("".into());
            ui.set_schedule_label_closable(false);
        }
    });

    ui.on_schedule_opened({
        let ui_weak = ui.as_weak();
        let state = state.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let playing = {
                let s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.station_url.clone().map(|url| ScheduledStation {
                    name: s.station_name.clone().unwrap_or_default(),
                    url,
                    logo_url: s.station_logo_url.clone(),
                    country: s.station_country.clone(),
                })
            };
            set_stations(&ui, offered_stations(&ui, playing));
            set_dates(&ui, Local::now().date_naive());
            // The list redraws from the state on the next poll
            SHOWN_SEQ.set(u64::MAX);
        }
    });

    ui.on_schedule_draft({
        let ui_weak = ui.as_weak();
        let state = state.clone();
        move |id| {
            let Some(ui) = ui_weak.upgrade() else {
                return ScheduleDraft::default();
            };
            let entry = (id > 0)
                .then(|| {
                    let s = state.lock().unwrap_or_else(|e| e.into_inner());
                    s.schedule.iter().find(|e| e.id == id as u64).cloned()
                })
                .flatten();
            draft_of(&ui, entry.as_ref())
        }
    });

    ui.on_schedule_save({
        let state = state.clone();
        let cmd_tx = cmd_tx.clone();
        move |draft| {
            let entries = state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .schedule
                .clone();
            match entry_of(&draft, &entries) {
                Ok(entry) => {
                    cmd_tx.send(AppCommand::SaveScheduleEntry { entry, reply: None });
                    SharedString::new()
                }
                Err(e) => e.into(),
            }
        }
    });

    ui.on_schedule_toggled({
        let cmd_tx = cmd_tx.clone();
        move |id, enabled| {
            cmd_tx.send(AppCommand::SetScheduleEntryEnabled {
                id: id as u64,
                enabled,
                reply: None,
            })
        }
    });

    ui.on_schedule_remove(move |id| {
        cmd_tx.send(AppCommand::RemoveScheduleEntry {
            id: id as u64,
            reply: None,
        })
    });
}

/// Favorites first, in the window's order, then the station playing if it
/// isn't one
fn offered_stations(ui: &App, playing: Option<ScheduledStation>) -> Vec<ScheduledStation> {
    let mut stations: Vec<ScheduledStation> = ui
        .get_favorites_list()
        .iter()
        .map(|f| ScheduledStation {
            name: f.name.to_string(),
            url: f.url.to_string(),
            logo_url: Some(f.logo_url.to_string()).filter(|l| !l.is_empty()),
            country: Some(f.country.to_string()).filter(|c| !c.is_empty()),
        })
        .collect();
    if let Some(playing) = playing {
        if !stations.iter().any(|s| s.url == playing.url) {
            stations.push(playing);
        }
    }
    stations
}

fn set_stations(ui: &App, stations: Vec<ScheduledStation>) {
    let names: Vec<SharedString> = stations
        .iter()
        .map(|s| {
            if s.name.is_empty() {
                s.url.as_str().into()
            } else {
                s.name.as_str().into()
            }
        })
        .collect();
    ui.set_schedule_stations(ModelRc::from(Rc::new(VecModel::from(names))));
    STATIONS.with(|list| *list.borrow_mut() = stations);
}

/// The days a one-off can be set for, and today's place in the week
fn set_dates(ui: &App, today: NaiveDate) {
    let dates: Vec<SharedString> = (0..SCHEDULE_DATES)
        .map(|n| date_label(today, today + TimeDelta::days(n)).into())
        .collect();
    ui.set_schedule_dates(ModelRc::from(Rc::new(VecModel::from(dates))));
    ui.set_schedule_today(today.weekday().num_days_from_monday() as i32);
}

/// "Today, Fri 3 Oct", "Tomorrow, Sat 4 Oct", "Sun 5 Oct"
fn date_label(today: NaiveDate, date: NaiveDate) -> String {
    let day = date.format("%a %-d %b");
    match (date - today).num_days() {
        0 => format!("Today, {day}"),
        1 => format!("Tomorrow, {day}"),
        _ => day.to_string(),
    }
}

/// Only today's day picked, for a switch to Weekly
fn today_only(today: NaiveDate) -> [bool; 7] {
    let mut days = [false; 7];
    days[today.weekday().num_days_from_monday() as usize] = true;
    days
}

/// A new entry starts at the next full hour: today, or tomorrow when that
/// is past midnight
fn next_full_hour(now: &DateTime<Local>) -> (ClockTime, i32) {
    let hour = (now.hour() + 1) % 24;
    let start = ClockTime::new(hour, 0).unwrap_or_default();
    (start, if hour == 0 { 1 } else { 0 })
}

/// An hour after `start`, for an end at a time
fn hour_after(start: ClockTime) -> String {
    ClockTime::new((start.hour() + 1) % 24, start.minute())
        .unwrap_or(start)
        .to_string()
}

/// The dialog's form for `entry`, or a new one (Play once, at the next
/// full hour, for the station playing)
fn draft_of(ui: &App, entry: Option<&Entry>) -> ScheduleDraft {
    let now = Local::now();
    let today = now.date_naive();
    set_dates(ui, today);
    let Some(entry) = entry else {
        let playing = ui.get_station_url();
        let station = STATIONS.with(|list| {
            list.borrow()
                .iter()
                .position(|s| s.url == playing.as_str())
                .unwrap_or(0)
        });
        let (start, date) = next_full_hour(&now);
        return ScheduleDraft {
            id: 0,
            action: "play".into(),
            station: station as i32,
            start: start.to_string().into(),
            once: true,
            date,
            days: ModelRc::from(Rc::new(VecModel::from(today_only(today).to_vec()))),
            end_kind: "never".into(),
            end_after: DEFAULT_MINUTES.to_string().into(),
            end_at: hour_after(start).into(),
            volume: 0,
            fade: true,
            fallback: true,
        };
    };
    // The entry's station is offered even when it is no favorite now
    let station = entry.station.as_ref().map(|station| {
        STATIONS.with(|list| {
            let mut list = list.borrow_mut();
            match list.iter().position(|s| s.url == station.url) {
                Some(i) => i,
                None => {
                    list.push(station.clone());
                    list.len() - 1
                }
            }
        })
    });
    let stations = STATIONS.with(|list| list.borrow().clone());
    set_stations(ui, stations);
    let (end_kind, end_after, end_at) = match entry.end {
        End::Never => (
            "never",
            DEFAULT_MINUTES.to_string(),
            hour_after(entry.start),
        ),
        End::After { minutes } => ("after", minutes.to_string(), hour_after(entry.start)),
        End::At { time } => ("at", DEFAULT_MINUTES.to_string(), time.to_string()),
    };
    let once = entry.days.is_once();
    let date = entry
        .date
        .map_or(0, |d| (d - today).num_days().clamp(0, SCHEDULE_DATES - 1)) as i32;
    ScheduleDraft {
        id: entry.id as i32,
        action: entry.action.id().into(),
        station: station.unwrap_or(0) as i32,
        start: entry.start.to_string().into(),
        once,
        date,
        days: if once {
            ModelRc::from(Rc::new(VecModel::from(today_only(today).to_vec())))
        } else {
            days_model(entry.days)
        },
        end_kind: end_kind.into(),
        end_after: end_after.into(),
        end_at: end_at.into(),
        volume: volume_index(entry.volume),
        fade: entry.fade,
        fallback: entry.fallback,
    }
}

fn days_model(days: Days) -> ModelRc<bool> {
    ModelRc::from(Rc::new(VecModel::from(days.flags().to_vec())))
}

/// The Volume list's index for `volume`: Keep current, Silent, 10%..100%
fn volume_index(volume: Option<f32>) -> i32 {
    match volume {
        None => 0,
        Some(v) if v <= 0.0 => 1,
        Some(v) => {
            let percent = (v * 100.0).round() as u32;
            VOLUMES
                .iter()
                .position(|p| *p >= percent)
                .map_or(VOLUMES.len() as i32 + 1, |i| i as i32 + 2)
        }
    }
}

fn volume_of(index: i32) -> Option<f32> {
    match index {
        i if i <= 0 => None,
        1 => Some(0.0),
        i => VOLUMES.get(i as usize - 2).map(|p| *p as f32 / 100.0),
    }
}

/// The entry the dialog's form describes, or what is wrong with it
fn entry_of(draft: &ScheduleDraft, entries: &[Entry]) -> Result<Entry, String> {
    entry_at(draft, entries, &Local::now())
}

fn entry_at(
    draft: &ScheduleDraft,
    entries: &[Entry],
    now: &DateTime<Local>,
) -> Result<Entry, String> {
    let action = Action::from_id(draft.action.as_str()).ok_or("Pick what to do")?;
    let start = ClockTime::parse(draft.start.as_str())
        .ok_or("Type the start as hours and minutes, like 07:30")?;
    let mut days = [false; 7];
    if !draft.once {
        for (day, on) in days.iter_mut().zip(draft.days.iter()) {
            *day = on;
        }
        if !days.contains(&true) {
            return Err("Pick at least one day".into());
        }
    }
    let today = now.date_naive();
    let date = draft
        .once
        .then(|| today + TimeDelta::days(draft.date.clamp(0, SCHEDULE_DATES as i32 - 1).into()));
    if date == Some(today) && schedule::local_at(&Local, today, start) <= *now {
        return Err(format!(
            "{start} has already passed today. Pick a later time or another day."
        ));
    }
    let end = match (action, draft.end_kind.as_str()) {
        (Action::Stop, _) | (_, "never") => End::Never,
        (_, "after") => {
            let minutes: u32 = draft
                .end_after
                .trim()
                .parse()
                .map_err(|_| "Type the length in minutes, like 60".to_string())?;
            End::After { minutes }
        }
        _ => End::At {
            time: ClockTime::parse(draft.end_at.as_str())
                .ok_or("Type the end as hours and minutes, like 22:00")?,
        },
    };
    let station = if action == Action::Stop {
        None
    } else {
        let station = STATIONS.with(|list| list.borrow().get(draft.station as usize).cloned());
        Some(station.ok_or("Pick a station")?)
    };
    let entry = Entry {
        id: draft.id.max(0) as u64,
        enabled: true,
        action,
        station,
        start,
        days: Days::from_flags(days),
        date,
        end,
        volume: volume_of(draft.volume),
        fade: draft.fade && action != Action::Record,
        fallback: draft.fallback,
        armed_from: 0,
    }
    .tidied(now);
    entry.check()?;
    if let Some(other) = schedule::clashing_recording(entries, &entry, today, &Local) {
        return Err(clash_message(other));
    }
    Ok(entry)
}

/// What the window shows of the schedule, copied under the state's lock
pub struct View {
    sleep: Option<SleepTimerInfo>,
    alarm_beep: bool,
    scheduled: Option<ScheduledNow>,
    next_run: Option<NextRun>,
    /// The list's version, and the entries when it is not the one shown
    seq: u64,
    entries: Option<Vec<Entry>>,
}

impl View {
    pub fn of(s: &AppSnapshot) -> Self {
        let running_id = s.scheduled.as_ref().map(|r| r.id);
        let seq = s.schedule_seq ^ running_id.unwrap_or(0).rotate_left(32);
        View {
            sleep: s.sleep.clone(),
            alarm_beep: s.alarm_beep,
            scheduled: s.scheduled.clone(),
            next_run: s.next_run.clone(),
            seq,
            entries: (SHOWN_SEQ.get() != seq).then(|| s.schedule.clone()),
        }
    }
}

/// Mirror the schedule and the sleep timer into the window (called every
/// 200 ms)
pub fn show_state(ui: &App, view: View) {
    let now = Local::now();
    match &view.sleep {
        Some(sleep) => {
            let left = (sleep.until - now).num_seconds().max(0);
            ui.set_sleep_active(true);
            ui.set_sleep_left(clock_text(left).into());
            ui.set_sleep_left_secs(left as i32);
            ui.set_sleep_length(sleep.minutes as i32);
            ui.set_sleep_until(sleep.until.format("%H:%M").to_string().into());
        }
        None => {
            if ui.get_sleep_active() {
                ui.set_sleep_active(false);
            }
        }
    }

    let label = schedule_label(&view, &now);
    if ui.get_schedule_label() != label.text.as_str() {
        ui.set_schedule_label(label.text.into());
    }
    if ui.get_schedule_label_bell() != label.bell {
        ui.set_schedule_label_bell(label.bell);
    }
    if ui.get_schedule_label_closable() != label.closable {
        ui.set_schedule_label_closable(label.closable);
    }

    let next = view
        .next_run
        .as_ref()
        .map(|n| format!("Next: {}, {}", schedule::when_label(&n.at, &now), n.title))
        .unwrap_or_default();
    if ui.get_schedule_next() != next.as_str() {
        ui.set_schedule_next(next.into());
    }

    // The list, when it changed or what runs did
    if let Some(entries) = view.entries {
        SHOWN_SEQ.set(view.seq);
        let running_id = view.scheduled.as_ref().map(|r| r.id);
        ui.set_schedule_rows(ModelRc::from(Rc::new(VecModel::from(rows(
            &entries, running_id,
        )))));
    }
}

/// Close the label that says what started, or the fallback sound one
fn dismiss_label(bell: bool) {
    if bell {
        BEEP_DISMISSED.set(true);
    } else {
        STARTED_DISMISSED.set(true);
    }
}

/// The Scheduler's label: a countdown before an entry, what started just
/// after, or the fallback sound playing. The last two close on a click.
fn schedule_label(view: &View, now: &DateTime<Local>) -> Label {
    if view.alarm_beep {
        if !BEEP_DISMISSED.get() {
            return Label {
                text: "Fallback sound".into(),
                bell: true,
                closable: true,
            };
        }
    } else {
        BEEP_DISMISSED.set(false);
    }
    if let Some(next) = &view.next_run {
        let left = (next.at - *now).num_milliseconds();
        if left > 0 && left <= SCHEDULE_COUNTDOWN_SECS * 1000 {
            COUNTED_DOWN.with(|c| *c.borrow_mut() = Some(next.clone()));
            STARTED_DISMISSED.set(false);
            // Rounded up: "in 0:01" until it starts
            let secs = (left + 999) / 1000;
            return Label {
                text: format!("{} in {}", verb(next.action), clock_text(secs)),
                ..Label::default()
            };
        }
    }
    let started = COUNTED_DOWN.with(|c| c.borrow().clone());
    if let Some(entry) = started {
        let since = (*now - entry.at).num_seconds();
        // A play or recording that was skipped (a recording ran) says nothing
        let ran = entry.action == Action::Stop
            || view.scheduled.as_ref().is_some_and(|s| s.id == entry.id);
        if (0..SCHEDULE_LABEL_SECS).contains(&since) && ran && !STARTED_DISMISSED.get() {
            return Label {
                text: match entry.action {
                    Action::Stop => "Stopped playback".into(),
                    action => format!("{} · {}", verb(action), entry.station),
                },
                bell: false,
                closable: true,
            };
        }
        if since >= SCHEDULE_LABEL_SECS {
            COUNTED_DOWN.with(|c| *c.borrow_mut() = None);
        }
    }
    Label::default()
}

fn verb(action: Action) -> &'static str {
    match action {
        Action::Play => "Play",
        Action::Record => "Record",
        Action::Stop => "Stop",
    }
}

/// The station's name, or what a Stop does, for the list
fn row_title(entry: &Entry) -> String {
    match &entry.station {
        _ if entry.action == Action::Stop => "Stop playback".into(),
        Some(station) if !station.name.is_empty() => station.name.clone(),
        Some(station) => station.url.clone(),
        None => "No station".into(),
    }
}

/// The list's rows, by start time
fn rows(entries: &[Entry], running: Option<u64>) -> Vec<ScheduleRow> {
    let mut entries: Vec<&Entry> = entries.iter().collect();
    entries.sort_by_key(|e| (e.start, e.id));
    entries
        .into_iter()
        .map(|e| ScheduleRow {
            id: e.id as i32,
            enabled: e.enabled,
            time: e.start.to_string().into(),
            action: e.action.id().into(),
            title: row_title(e).into(),
            details: e.details().into(),
            running: running == Some(e.id),
        })
        .collect()
}

/// "4:05", "23:12" or "1:02:03"
fn clock_text(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn volumes_round_trip_through_the_list() {
        assert_eq!(volume_index(None), 0);
        assert_eq!(volume_index(Some(0.0)), 1);
        assert_eq!(volume_index(Some(0.4)), 5);
        assert_eq!(volume_of(5), Some(0.4));
        assert_eq!(volume_of(0), None);
        assert_eq!(volume_of(1), Some(0.0));
        assert_eq!(volume_of(11), Some(1.0));
        // A volume set by an agent shows as the next step up
        assert_eq!(volume_index(Some(0.35)), 5);
    }

    #[test]
    fn a_new_entry_starts_at_the_next_full_hour() {
        let at = |h, m| Local.with_ymd_and_hms(2026, 10, 3, h, m, 0).unwrap();
        assert_eq!(
            next_full_hour(&at(15, 52)),
            (ClockTime::new(16, 0).unwrap(), 0)
        );
        assert_eq!(
            next_full_hour(&at(15, 0)),
            (ClockTime::new(16, 0).unwrap(), 0)
        );
        // Past midnight: tomorrow
        assert_eq!(
            next_full_hour(&at(23, 10)),
            (ClockTime::new(0, 0).unwrap(), 1)
        );
        assert_eq!(hour_after(ClockTime::new(23, 30).unwrap()), "00:30");
        let today = at(12, 0).date_naive();
        assert_eq!(date_label(today, today), "Today, Sat 3 Oct");
        assert_eq!(
            date_label(today, today + TimeDelta::days(1)),
            "Tomorrow, Sun 4 Oct"
        );
        assert_eq!(date_label(today, today + TimeDelta::days(2)), "Mon 5 Oct");
        assert_eq!(
            today_only(today),
            [false, false, false, false, false, true, false]
        );
    }

    #[test]
    fn the_scheduler_label_counts_down_then_says_what_started() {
        let at = Local.with_ymd_and_hms(2026, 10, 3, 7, 0, 0).unwrap();
        let next = NextRun {
            id: 4,
            at,
            title: "Play Jazz FM".into(),
            action: Action::Play,
            station: "Jazz FM".into(),
        };
        let view = |next: Option<NextRun>, running: Option<u64>, beep: bool| View {
            sleep: None,
            alarm_beep: beep,
            scheduled: running.map(|id| ScheduledNow {
                id,
                title: String::new(),
                until: None,
                record: false,
            }),
            next_run: next,
            seq: 0,
            entries: None,
        };
        COUNTED_DOWN.with(|c| *c.borrow_mut() = None);
        let label = |v: &View, secs: i64| schedule_label(v, &(at + TimeDelta::seconds(secs))).text;

        assert_eq!(label(&view(Some(next.clone()), None, false), -61), "");
        assert_eq!(
            label(&view(Some(next.clone()), None, false), -60),
            "Play in 1:00"
        );
        assert_eq!(
            label(&view(Some(next.clone()), None, false), -1),
            "Play in 0:01"
        );
        // The countdown opens the Scheduler; what started closes on a click
        assert!(
            !schedule_label(
                &view(Some(next.clone()), None, false),
                &(at - TimeDelta::seconds(5))
            )
            .closable
        );
        // Started: the next run moved on to tomorrow
        assert_eq!(label(&view(None, Some(4), false), 0), "Play · Jazz FM");
        assert_eq!(label(&view(None, Some(4), false), 14), "Play · Jazz FM");
        assert!(
            schedule_label(&view(None, Some(4), false), &(at + TimeDelta::seconds(3))).closable
        );
        assert_eq!(label(&view(None, Some(4), false), 15), "");
        assert_eq!(label(&view(None, Some(4), true), 30), "Fallback sound");
        assert!(schedule_label(&view(None, None, true), &at).bell);

        // Skipped: nothing claims it started
        label(&view(Some(next.clone()), None, false), -5);
        assert_eq!(label(&view(None, None, false), 1), "");

        // Clicked away: what started stays closed, the next countdown shows
        label(&view(Some(next.clone()), None, false), -5);
        assert_eq!(label(&view(None, Some(4), false), 1), "Play · Jazz FM");
        dismiss_label(false);
        assert_eq!(label(&view(None, Some(4), false), 2), "");
        assert_eq!(
            label(&view(Some(next.clone()), None, false), -5),
            "Play in 0:05"
        );
        assert_eq!(label(&view(None, Some(4), false), 1), "Play · Jazz FM");

        // The fallback sound label stays closed until the beep ends
        assert_eq!(label(&view(None, Some(4), true), 30), "Fallback sound");
        dismiss_label(true);
        assert_eq!(label(&view(None, Some(4), true), 31), "");
        label(&view(None, Some(4), false), 40);
        assert_eq!(label(&view(None, Some(4), true), 41), "Fallback sound");
    }

    #[test]
    fn time_left_reads_like_a_clock() {
        assert_eq!(clock_text(245), "4:05");
        assert_eq!(clock_text(23 * 60 + 12), "23:12");
        assert_eq!(clock_text(3723), "1:02:03");
    }

    #[test]
    fn a_form_becomes_an_entry_or_says_why_not() {
        STATIONS.with(|list| {
            *list.borrow_mut() = vec![ScheduledStation {
                name: "Jazz FM".into(),
                url: "http://jazz.test/stream".into(),
                ..Default::default()
            }]
        });
        let draft = |action: &str, end_kind: &str| ScheduleDraft {
            id: 0,
            action: action.into(),
            station: 0,
            start: "20:00".into(),
            once: false,
            date: 0,
            days: days_model(Days::WEEKENDS),
            end_kind: end_kind.into(),
            end_after: "90".into(),
            end_at: "22:00".into(),
            volume: 0,
            fade: true,
            fallback: true,
        };
        let entry = entry_of(&draft("record", "at"), &[]).unwrap();
        assert_eq!(
            entry.end,
            End::At {
                time: ClockTime::new(22, 0).unwrap()
            }
        );
        assert!(!entry.fade, "a recording has no fade");
        assert_eq!(entry.station.unwrap().name, "Jazz FM");

        let entry = entry_of(&draft("stop", "after"), &[]).unwrap();
        assert_eq!(entry.end, End::Never);
        assert!(entry.station.is_none());

        assert_eq!(
            entry_of(&draft("record", "never"), &[]).unwrap_err(),
            "A recording needs an end"
        );
        let mut bad = draft("play", "after");
        bad.start = "7".into();
        assert!(entry_of(&bad, &[])
            .unwrap_err()
            .starts_with("Type the start"));
        bad = draft("play", "after");
        bad.end_after = "an hour".into();
        assert!(entry_of(&bad, &[])
            .unwrap_err()
            .starts_with("Type the length"));

        let mut weekly = draft("play", "never");
        weekly.days = days_model(Days::ONCE);
        assert_eq!(entry_of(&weekly, &[]).unwrap_err(), "Pick at least one day");

        // Once, today or a day picked further on; a time gone today is refused
        let now = Local.with_ymd_and_hms(2026, 10, 3, 15, 52, 0).unwrap();
        let mut once = draft("play", "never");
        once.once = true;
        once.start = "16:00".into();
        let entry = entry_at(&once, &[], &now).unwrap();
        assert!(entry.days.is_once());
        assert_eq!(entry.date, Some(now.date_naive()));
        once.start = "15:00".into();
        assert!(entry_at(&once, &[], &now)
            .unwrap_err()
            .starts_with("15:00 has already passed today"));
        once.date = 2;
        let entry = entry_at(&once, &[], &now).unwrap();
        assert_eq!(entry.date, Some(now.date_naive() + TimeDelta::days(2)));
        once.fallback = false;
        assert!(!entry_at(&once, &[], &now).unwrap().fallback);

        // A second recording at the same time
        let mut first = entry_of(&draft("record", "at"), &[]).unwrap();
        first.id = 1;
        let error = entry_of(&draft("record", "after"), &[first]).unwrap_err();
        assert!(error.starts_with("Overlaps Record Jazz FM"), "{error}");
    }
}
