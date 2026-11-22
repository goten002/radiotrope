//! "Quit Radiotrope?": asked once on close or Quit when closing would
//! cancel something the user set up, a recording running now or a
//! Scheduler entry due within a day. Never on logout, SIGTERM or
//! `radiotrope --quit`: those end the app at once.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone};
use slint::ComponentHandle;

use radiotrope_app::data::schedule::{Action, Entry};

use crate::app::state::AppSnapshot;
use crate::context::lock;
use crate::App;

/// Entries starting within this long count: one due next week isn't lost
/// by closing tonight
const SCHEDULE_HORIZON_HOURS: i64 = 24;

/// What closing now would cancel, one line each; empty when it cancels
/// nothing of that kind
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct QuitQuestion {
    pub recording: String,
    pub schedule: String,
}

/// The question to ask before quitting at `now`, or `None` when the app
/// can quit at once
pub(crate) fn quit_question<Tz: TimeZone>(
    state: &AppSnapshot,
    now: &DateTime<Tz>,
) -> Option<QuitQuestion> {
    let recording = if state.recording.is_some() {
        recording_line(state.station_name.as_deref())
    } else {
        String::new()
    };
    let schedule = {
        let mut due = due_soon(&state.schedule, now);
        due.sort_by(|a, b| a.0.cmp(&b.0));
        match due.first() {
            Some((at, entry)) => schedule_line(due.len(), &entry.title(), &day_and_time(at, now)),
            None => String::new(),
        }
    };
    (!recording.is_empty() || !schedule.is_empty()).then_some(QuitQuestion {
        recording,
        schedule,
    })
}

/// The enabled entries that would start within the next day, with their
/// start. A Stop entry is left out: closing the app stops playback anyway.
fn due_soon<'a, Tz: TimeZone>(
    entries: &'a [Entry],
    now: &DateTime<Tz>,
) -> Vec<(DateTime<Tz>, &'a Entry)> {
    let horizon = now.clone() + Duration::hours(SCHEDULE_HORIZON_HOURS);
    entries
        .iter()
        .filter(|entry| entry.action != Action::Stop)
        .filter_map(|entry| entry.next_start(now).map(|start| (start, entry)))
        .filter(|(start, _)| *start <= horizon)
        .collect()
}

/// "Recording Harbour Jazz will stop."
pub(crate) fn recording_line(station: Option<&str>) -> String {
    match station.filter(|name| !name.is_empty()) {
        Some(name) => format!("Recording {name} will stop."),
        None => "The recording will stop.".into(),
    }
}

/// "Record Harbour Jazz won't run while Radiotrope is closed (tomorrow at
/// 07:00)." or, for several, "2 scheduled entries won't run while
/// Radiotrope is closed. Next: Record Harbour Jazz, tomorrow at 07:00."
pub(crate) fn schedule_line(count: usize, next_title: &str, next_when: &str) -> String {
    if count == 1 {
        format!("{next_title} won't run while Radiotrope is closed ({next_when}).")
    } else {
        format!(
            "{count} scheduled entries won't run while Radiotrope is closed. \
             Next: {next_title}, {next_when}."
        )
    }
}

/// "today at 07:00", "tomorrow at 07:00" (within a day, nothing later)
fn day_and_time<Tz: TimeZone>(at: &DateTime<Tz>, now: &DateTime<Tz>) -> String {
    let time = at.naive_local().format("%H:%M");
    if at.date_naive() == now.date_naive() {
        format!("today at {time}")
    } else {
        format!("tomorrow at {time}")
    }
}

/// Close or Quit, asked for by the user: quit at once, or ask first
pub(crate) fn request_quit(ui: &App, state: &Arc<Mutex<AppSnapshot>>) {
    if ui.get_quit_shown() {
        return;
    }
    let question = quit_question(&lock(state), &chrono::Local::now());
    let Some(question) = question else {
        crate::quit_app();
        return;
    };
    ui.set_quit_recording_line(question.recording.into());
    ui.set_quit_schedule_line(question.schedule.into());
    #[cfg(feature = "desktop")]
    ui.set_quit_can_keep_running(crate::tray::has_tray(ui));
    // Quit in the tray's menu while the window is hidden
    crate::bring_to_front(ui.window());
    ui.set_quit_shown(true);
}

/// Whether a close of the window is held while the question is up
pub(crate) fn asking(ui: &App) -> bool {
    ui.get_quit_shown()
}

/// The question's buttons, and Quit in the menu, Ctrl+Q and the tray
pub(crate) fn setup(ui: &App, state: &Arc<Mutex<AppSnapshot>>) {
    ui.on_quit_confirmed(crate::quit_app);
    let ui_weak = ui.as_weak();
    ui.on_quit_keep_running(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let _ = ui.hide();
        }
    });
    #[cfg(feature = "desktop")]
    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_quit_app(move || {
            if let Some(ui) = ui_weak.upgrade() {
                request_quit(&ui, &state);
            }
        });
    }
    #[cfg(not(feature = "desktop"))]
    let _ = state;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::RecordingProgress;
    use chrono::{NaiveDate, Utc};
    use radiotrope_app::data::schedule::{ClockTime, Days, End, ScheduledStation};

    /// Friday 20 November 2026, 22:00
    fn now() -> DateTime<Utc> {
        Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(2026, 11, 20)
                .unwrap()
                .and_hms_opt(22, 0, 0)
                .unwrap(),
        )
    }

    fn entry(action: Action, start: &str, days: Days) -> Entry {
        Entry {
            id: 1,
            enabled: true,
            action,
            station: (action != Action::Stop).then(|| ScheduledStation {
                name: "Harbour Jazz".into(),
                url: "http://example.com/jazz".into(),
                ..Default::default()
            }),
            start: ClockTime::parse(start).unwrap(),
            days,
            date: None,
            end: End::Never,
            volume: None,
            fade: false,
            fallback: true,
            armed_from: 0,
        }
    }

    fn recording() -> AppSnapshot {
        AppSnapshot {
            station_name: Some("Harbour Jazz".into()),
            recording: Some(RecordingProgress {
                path: "/music/jazz.mp3".into(),
                duration: std::time::Duration::from_secs(60),
                bytes: 1000,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn nothing_to_lose_quits_at_once() {
        assert_eq!(quit_question(&AppSnapshot::default(), &now()), None);
    }

    #[test]
    fn a_recording_asks() {
        let question = quit_question(&recording(), &now()).unwrap();
        assert_eq!(question.recording, "Recording Harbour Jazz will stop.");
        assert_eq!(question.schedule, "");
    }

    #[test]
    fn an_entry_due_within_a_day_asks() {
        let state = AppSnapshot {
            schedule: vec![entry(Action::Play, "07:00", Days::EVERY_DAY)],
            ..Default::default()
        };
        let question = quit_question(&state, &now()).unwrap();
        assert_eq!(question.recording, "");
        assert_eq!(
            question.schedule,
            "Play Harbour Jazz won't run while Radiotrope is closed (tomorrow at 07:00)."
        );
    }

    #[test]
    fn several_entries_name_the_next_one() {
        let state = AppSnapshot {
            schedule: vec![
                entry(Action::Play, "07:00", Days::EVERY_DAY),
                entry(Action::Record, "23:30", Days::EVERY_DAY),
            ],
            ..recording()
        };
        let question = quit_question(&state, &now()).unwrap();
        assert_eq!(question.recording, "Recording Harbour Jazz will stop.");
        assert_eq!(
            question.schedule,
            "2 scheduled entries won't run while Radiotrope is closed. \
             Next: Record Harbour Jazz, today at 23:30."
        );
    }

    #[test]
    fn entries_later_off_or_stop_only_dont_ask() {
        let mut off = entry(Action::Play, "07:00", Days::EVERY_DAY);
        off.enabled = false;
        // Friday 22:00: the next Wednesday is days away
        let wednesday = Days::from_flags([false, false, true, false, false, false, false]);
        let state = AppSnapshot {
            schedule: vec![
                off,
                entry(Action::Play, "07:00", wednesday),
                entry(Action::Stop, "23:00", Days::EVERY_DAY),
            ],
            ..Default::default()
        };
        assert_eq!(quit_question(&state, &now()), None);
    }

    #[test]
    fn the_day_boundary_is_24_hours() {
        // Friday 22:00: Saturday 21:59 is within the day, 22:01 is not
        let saturday = Days::from_flags([false, false, false, false, false, true, false]);
        let state = |start| AppSnapshot {
            schedule: vec![entry(Action::Play, start, saturday)],
            ..Default::default()
        };
        assert!(quit_question(&state("21:59"), &now()).is_some());
        assert_eq!(quit_question(&state("22:01"), &now()), None);
    }
}
