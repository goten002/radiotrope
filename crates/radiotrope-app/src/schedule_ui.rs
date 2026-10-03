//! The window's side of the schedule: the Sleep Timer and Schedule
//! dialogs, and the chips that show what runs
//!
//! The controller keeps the entries and runs them; this sends it the
//! changes and shows what it has. A draft is checked here first, with the
//! same rules, so the dialog can say at once what is wrong with it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use chrono::Local;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use radiotrope_app::data::schedule::{self, Action, ClockTime, Days, End, Entry, ScheduledStation};

use crate::app::controller::clash_message;
use crate::app::state::{AppCommand, AppSnapshot, NextRun, ScheduledNow, SleepTimerInfo};
use crate::app::ui_sender::UiSender;
use crate::{App, ScheduleDraft, ScheduleRow};

/// Volumes the dialog offers after "Keep current" and "Silent", in percent
const VOLUMES: [u32; 10] = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];

/// Length of a new entry's play or recording, in minutes
const DEFAULT_MINUTES: u32 = 60;

thread_local! {
    /// The stations the open dialog offers, in its list's order
    static STATIONS: RefCell<Vec<ScheduledStation>> = const { RefCell::new(Vec::new()) };
    /// The schedule shown, by `schedule_seq`
    static SHOWN_SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
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
    ui.on_sleep_extend({
        let cmd_tx = cmd_tx.clone();
        move |minutes| cmd_tx.send(AppCommand::ExtendSleepTimer(minutes.max(1) as u32))
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

/// The dialog's form for `entry`, or a new one (an alarm at 07:00 on
/// weekdays, for the station playing)
fn draft_of(ui: &App, entry: Option<&Entry>) -> ScheduleDraft {
    let Some(entry) = entry else {
        let playing = ui.get_station_url();
        let station = STATIONS.with(|list| {
            list.borrow()
                .iter()
                .position(|s| s.url == playing.as_str())
                .unwrap_or(0)
        });
        return ScheduleDraft {
            id: 0,
            action: "play".into(),
            station: station as i32,
            start: "07:00".into(),
            days: days_model(Days::WEEKDAYS),
            end_kind: "never".into(),
            end_after: DEFAULT_MINUTES.to_string().into(),
            end_at: String::new().into(),
            volume: 0,
            fade: true,
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
        End::Never => ("never", DEFAULT_MINUTES.to_string(), String::new()),
        End::After { minutes } => ("after", minutes.to_string(), String::new()),
        End::At { time } => ("at", DEFAULT_MINUTES.to_string(), time.to_string()),
    };
    ScheduleDraft {
        id: entry.id as i32,
        action: entry.action.id().into(),
        station: station.unwrap_or(0) as i32,
        start: entry.start.to_string().into(),
        days: days_model(entry.days),
        end_kind: end_kind.into(),
        end_after: end_after.into(),
        end_at: end_at.into(),
        volume: volume_index(entry.volume),
        fade: entry.fade,
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
    let action = Action::from_id(draft.action.as_str()).ok_or("Pick what to do")?;
    let start = ClockTime::parse(draft.start.as_str())
        .ok_or("Type the start as hours and minutes, like 07:30")?;
    let flags: Vec<bool> = draft.days.iter().collect();
    let mut days = [false; 7];
    for (day, on) in days.iter_mut().zip(flags) {
        *day = on;
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
    let previous = entries.iter().find(|e| e.id == draft.id as u64);
    let entry = Entry {
        id: draft.id.max(0) as u64,
        enabled: true,
        action,
        station,
        start,
        days: Days::from_flags(days),
        // A one-off keeps the day it was set for, if still ahead
        date: previous.and_then(|e| e.date),
        end,
        volume: volume_of(draft.volume),
        fade: draft.fade && action != Action::Record,
        armed_from: 0,
    }
    .tidied(&Local::now());
    entry.check()?;
    if let Some(other) =
        schedule::clashing_recording(entries, &entry, Local::now().date_naive(), &Local)
    {
        return Err(clash_message(other));
    }
    Ok(entry)
}

/// What the window shows of the schedule, copied under the state's lock
pub struct View {
    sleep: Option<SleepTimerInfo>,
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
            ui.set_sleep_until(sleep.until.format("%H:%M").to_string().into());
        }
        None => {
            if ui.get_sleep_active() {
                ui.set_sleep_active(false);
            }
        }
    }

    let (running, detail) = match &view.scheduled {
        Some(now_running) => (
            if now_running.record {
                "Scheduled".to_string()
            } else {
                "Alarm".to_string()
            },
            now_running
                .until
                .map(|until| format!("until {}", until.format("%H:%M")))
                .unwrap_or_default(),
        ),
        None => (String::new(), String::new()),
    };
    if ui.get_schedule_running() != running.as_str() {
        ui.set_schedule_running(running.into());
    }
    if ui.get_schedule_running_detail() != detail.as_str() {
        ui.set_schedule_running_detail(detail.into());
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
            title: e.title().into(),
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
            days: days_model(Days::WEEKENDS),
            end_kind: end_kind.into(),
            end_after: "90".into(),
            end_at: "22:00".into(),
            volume: 0,
            fade: true,
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

        // A second recording at the same time
        let mut first = entry_of(&draft("record", "at"), &[]).unwrap();
        first.id = 1;
        let error = entry_of(&draft("record", "after"), &[first]).unwrap_err();
        assert!(error.starts_with("Overlaps Record Jazz FM"), "{error}");
    }
}
