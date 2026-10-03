//! Scheduled playback: alarms, timed recordings and bedtime stops
//!
//! An entry plays a station (an alarm), plays and records it (a show), or
//! stops whatever plays (bedtime), at a time of day on chosen days or once.
//! A play or a recording can end after a while or at a time.
//!
//! Everything here works on a "now" it is given, so each rule is tested
//! without waiting for a clock. The controller asks about once a second
//! what is due, and does it.

use std::fmt;

use chrono::{DateTime, Datelike, LocalResult, NaiveDate, TimeDelta, TimeZone, Weekday};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The file the entries are kept in, next to the favorites
pub const FILE_NAME: &str = "schedules.json";

/// Version of the file's format
pub const FILE_VERSION: u32 = 1;

/// Most entries kept
pub const MAX_ENTRIES: usize = 100;

/// Longest play or recording, in minutes
pub const MAX_MINUTES: u32 = 24 * 60;

/// An alarm later than this doesn't start: music at 11:40 for a 07:00
/// alarm is worse than none. A recording still starts late.
pub const ALARM_GRACE_SECS: i64 = 10 * 60;

/// A bedtime stop later than this is dropped: stopping music started
/// after it would surprise
pub const STOP_GRACE_SECS: i64 = 2 * 60;

/// Runs missed longer ago than this pass without a word
pub const MISSED_NOTICE_SECS: i64 = 12 * 60 * 60;

/// What an entry does when it comes round
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Play the station
    Play,
    /// Play the station and record it
    Record,
    /// Stop whatever plays
    Stop,
}

impl Action {
    pub fn id(self) -> &'static str {
        match self {
            Action::Play => "play",
            Action::Record => "record",
            Action::Stop => "stop",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "play" => Some(Action::Play),
            "record" => Some(Action::Record),
            "stop" => Some(Action::Stop),
            _ => None,
        }
    }
}

/// A time of day, to the minute. Kept as "HH:MM" in the file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClockTime(u16);

impl ClockTime {
    pub fn new(hour: u32, minute: u32) -> Option<Self> {
        (hour < 24 && minute < 60).then(|| ClockTime((hour * 60 + minute) as u16))
    }

    /// "7:05", "07:05" or "7.05"
    pub fn parse(text: &str) -> Option<Self> {
        let (hour, minute) = text.trim().split_once([':', '.'])?;
        let hour = hour.trim();
        let minute = minute.trim();
        if hour.is_empty() || hour.len() > 2 || minute.len() != 2 {
            return None;
        }
        Self::new(hour.parse().ok()?, minute.parse().ok()?)
    }

    pub fn hour(self) -> u32 {
        u32::from(self.0) / 60
    }

    pub fn minute(self) -> u32 {
        u32::from(self.0) % 60
    }

    /// Minutes after midnight
    pub fn minutes(self) -> u32 {
        u32::from(self.0)
    }

    fn naive(self) -> chrono::NaiveTime {
        chrono::NaiveTime::from_hms_opt(self.hour(), self.minute(), 0).unwrap_or_default()
    }
}

impl fmt::Display for ClockTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour(), self.minute())
    }
}

impl Serialize for ClockTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ClockTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        ClockTime::parse(&text)
            .ok_or_else(|| serde::de::Error::custom(format!("not a time of day: {text:?}")))
    }
}

/// The days of the week an entry runs on. None picked is "once".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Days(u8);

const DAY_IDS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

impl Days {
    pub const ONCE: Days = Days(0);
    pub const WEEKDAYS: Days = Days(0b001_1111);
    pub const WEEKENDS: Days = Days(0b110_0000);
    pub const EVERY_DAY: Days = Days(0b111_1111);

    /// From seven flags, Monday first
    pub fn from_flags(flags: [bool; 7]) -> Self {
        let mut bits = 0;
        for (i, on) in flags.into_iter().enumerate() {
            if on {
                bits |= 1 << i;
            }
        }
        Days(bits)
    }

    /// Seven flags, Monday first
    pub fn flags(self) -> [bool; 7] {
        std::array::from_fn(|i| self.0 & (1 << i) != 0)
    }

    pub fn contains(self, day: Weekday) -> bool {
        self.0 & (1 << day.num_days_from_monday()) != 0
    }

    pub fn is_once(self) -> bool {
        self.0 == 0
    }

    /// "Every day", "Weekdays", "Weekends", "Mon, Wed" or "Once"
    pub fn label(self) -> String {
        match self {
            Days::ONCE => "Once".into(),
            Days::EVERY_DAY => "Every day".into(),
            Days::WEEKDAYS => "Weekdays".into(),
            Days::WEEKENDS => "Weekends".into(),
            _ => {
                let names: Vec<&str> = (0..7)
                    .filter(|i| self.0 & (1 << i) != 0)
                    .map(|i| DAY_NAMES[i])
                    .collect();
                // A single day reads as the plural: "Saturdays"
                match names.as_slice() {
                    [one] => format!("{}s", full_day_name(one)),
                    _ => names.join(", "),
                }
            }
        }
    }
}

fn full_day_name(short: &str) -> &'static str {
    match short {
        "Mon" => "Monday",
        "Tue" => "Tuesday",
        "Wed" => "Wednesday",
        "Thu" => "Thursday",
        "Fri" => "Friday",
        "Sat" => "Saturday",
        _ => "Sunday",
    }
}

impl Serialize for Days {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let ids: Vec<&str> = (0..7)
            .filter(|i| self.0 & (1 << i) != 0)
            .map(|i| DAY_IDS[i])
            .collect();
        ids.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Days {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let ids = Vec::<String>::deserialize(d)?;
        let mut bits = 0;
        for id in ids {
            let i = DAY_IDS
                .iter()
                .position(|d| d.eq_ignore_ascii_case(&id))
                .ok_or_else(|| serde::de::Error::custom(format!("not a day: {id:?}")))?;
            bits |= 1 << i;
        }
        Ok(Days(bits))
    }
}

/// When a play or a recording ends
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum End {
    /// Plays on until stopped
    #[default]
    Never,
    /// After this many minutes
    After { minutes: u32 },
    /// At this time; one before the start is the next day's
    At { time: ClockTime },
}

impl End {
    /// "", "for 1 h 30 min" or "until 22:00"
    pub fn label(self) -> String {
        match self {
            End::Never => String::new(),
            End::After { minutes } => format!("for {}", duration_label(minutes)),
            End::At { time } => format!("until {time}"),
        }
    }
}

/// "45 min", "1 h", "1 h 30 min"
pub fn duration_label(minutes: u32) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// The station an entry plays: its own copy, so removing the favorite
/// doesn't break it
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScheduledStation {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

/// One scheduled thing
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Given when the entry is first saved; 0 for a new one
    #[serde(default)]
    pub id: u64,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub action: Action,
    /// What Play and Record play; Stop has none
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub station: Option<ScheduledStation>,
    pub start: ClockTime,
    /// None picked: once, on `date`
    #[serde(default)]
    pub days: Days,
    /// The day a "once" entry runs
    #[serde(default, skip_serializing_if = "Option::is_none", with = "date_text")]
    pub date: Option<NaiveDate>,
    /// Play and Record only; a recording always has one
    #[serde(default)]
    pub end: End,
    /// Volume to play at (0 to 1); `None` keeps the current one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// Play: rise from silence. Stop: fade out first.
    #[serde(default)]
    pub fade: bool,
    /// Starts at or before this time (Unix seconds) don't run: the entry
    /// was made, changed or switched on after them, or already ran them
    #[serde(default)]
    pub armed_from: i64,
}

fn yes() -> bool {
    true
}

mod date_text {
    use chrono::NaiveDate;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(date: &Option<NaiveDate>, s: S) -> Result<S::Ok, S::Error> {
        match date {
            Some(date) => s.collect_str(&date.format("%Y-%m-%d")),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NaiveDate>, D::Error> {
        let Some(text) = Option::<String>::deserialize(d)? else {
            return Ok(None);
        };
        NaiveDate::parse_from_str(&text, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("not a date: {text:?}")))
    }
}

/// The local time `time` on `date`. A time skipped when the clocks go
/// forward is taken as the first minute after the gap; one that happens
/// twice when they go back, as the first of the two.
pub fn local_at<Tz: TimeZone>(tz: &Tz, date: NaiveDate, time: ClockTime) -> DateTime<Tz> {
    let mut at = date.and_time(time.naive());
    // Gaps are an hour, rarely more; a day is plenty
    for _ in 0..=24 * 60 {
        match tz.from_local_datetime(&at) {
            LocalResult::Single(t) => return t,
            LocalResult::Ambiguous(first, _) => return first,
            LocalResult::None => at += TimeDelta::minutes(1),
        }
    }
    // A zone with no times at all that day
    tz.from_utc_datetime(&date.and_time(time.naive()))
}

impl Entry {
    /// Whether it runs on `date`
    pub fn runs_on(&self, date: NaiveDate) -> bool {
        if self.days.is_once() {
            self.date == Some(date)
        } else {
            self.days.contains(date.weekday())
        }
    }

    /// Its start on `date`, if it runs that day
    pub fn start_on<Tz: TimeZone>(&self, tz: &Tz, date: NaiveDate) -> Option<DateTime<Tz>> {
        self.runs_on(date).then(|| local_at(tz, date, self.start))
    }

    /// When the play or recording that starts at `start` ends
    pub fn end_after<Tz: TimeZone>(&self, start: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        if self.action == Action::Stop {
            return None;
        }
        match self.end {
            End::Never => None,
            End::After { minutes } => Some(start.clone() + TimeDelta::minutes(minutes.into())),
            End::At { time } => {
                let tz = start.timezone();
                let date = start.date_naive();
                let end = local_at(&tz, date, time);
                if end > *start {
                    Some(end)
                } else {
                    date.succ_opt().map(|next| local_at(&tz, next, time))
                }
            }
        }
    }

    /// The latest start at or before `now` that hasn't run yet
    pub fn due<Tz: TimeZone>(&self, now: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        let tz = now.timezone();
        let today = now.date_naive();
        [Some(today), today.pred_opt()]
            .into_iter()
            .flatten()
            .filter_map(|date| self.start_on(&tz, date))
            .find(|start| start <= now && start.timestamp() > self.armed_from)
    }

    /// Its next start after `now`
    pub fn next_start<Tz: TimeZone>(&self, now: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        if !self.enabled {
            return None;
        }
        let tz = now.timezone();
        let today = now.date_naive();
        today
            .iter_days()
            .take(8)
            .filter_map(|date| self.start_on(&tz, date))
            .find(|start| start > now && start.timestamp() > self.armed_from)
    }

    /// "Play Jazz FM", "Record BBC Radio 3" or "Stop playback"
    pub fn title(&self) -> String {
        let name = self
            .station
            .as_ref()
            .map(|s| s.name.as_str())
            .filter(|n| !n.is_empty())
            .unwrap_or("a station");
        match self.action {
            Action::Play => format!("Play {name}"),
            Action::Record => format!("Record {name}"),
            Action::Stop => "Stop playback".into(),
        }
    }

    /// "Weekdays · for 1 h · fade in", for under the title
    pub fn details(&self) -> String {
        let mut parts = vec![match (self.days.is_once(), self.date) {
            (true, Some(date)) => date.format("%a %-d %b").to_string(),
            _ => self.days.label(),
        }];
        if self.action != Action::Stop {
            let end = self.end.label();
            if !end.is_empty() {
                parts.push(end);
            }
            if let Some(volume) = self.volume {
                parts.push(format!("volume {}%", (volume * 100.0).round() as u32));
            }
        }
        if self.fade {
            parts.push(if self.action == Action::Stop {
                "fade out".into()
            } else {
                "fade in".into()
            });
        }
        parts.join(" · ")
    }

    /// Whether it can be saved as it is, and why not
    pub fn check(&self) -> Result<(), String> {
        if self.action != Action::Stop {
            match &self.station {
                Some(station) if !station.url.trim().is_empty() => {}
                _ => return Err("Pick a station".into()),
            }
        }
        if self.action == Action::Record && self.end == End::Never {
            return Err("A recording needs an end".into());
        }
        if let End::After { minutes } = self.end {
            if minutes == 0 || minutes > MAX_MINUTES {
                return Err(format!(
                    "Pick a length from 1 min to {}",
                    duration_label(MAX_MINUTES)
                ));
            }
        }
        if let End::At { time } = self.end {
            if time == self.start {
                return Err("The end can't be the start time".into());
            }
        }
        if self.days.is_once() && self.date.is_none() {
            return Err("Pick the days it runs".into());
        }
        if let Some(volume) = self.volume {
            if !(0.0..=1.0).contains(&volume) {
                return Err("Volume must be from 0 to 100%".into());
            }
        }
        Ok(())
    }

    /// The same entry with what its action doesn't use cleared, and the
    /// date of a "once" entry set to the next time it comes round
    pub fn tidied<Tz: TimeZone>(mut self, now: &DateTime<Tz>) -> Self {
        if self.action == Action::Stop {
            self.station = None;
            self.end = End::Never;
            self.volume = None;
        }
        if self.days.is_once() {
            let tz = now.timezone();
            let today = now.date_naive();
            let start_today = local_at(&tz, today, self.start);
            let date = if start_today > *now {
                today
            } else {
                today.succ_opt().unwrap_or(today)
            };
            // A date picked further ahead is kept
            if self.date.is_none_or(|d| d < date) {
                self.date = Some(date);
            }
        } else {
            self.date = None;
        }
        self
    }

    /// Its plays (start, end) that touch the days from `first` to `last`
    fn windows<Tz: TimeZone>(
        &self,
        tz: &Tz,
        first: NaiveDate,
        last: NaiveDate,
    ) -> Vec<(DateTime<Tz>, DateTime<Tz>)> {
        first
            .iter_days()
            .take_while(|d| *d <= last)
            .filter_map(|date| {
                let start = self.start_on(tz, date)?;
                let end = self.end_after(&start)?;
                Some((start, end))
            })
            .collect()
    }
}

/// A recording among `entries` that would run at the same time as
/// `candidate`, which can only record one station at a time. Looks from
/// `today` a week and a day ahead, which covers every repeat.
pub fn clashing_recording<'a, Tz: TimeZone>(
    entries: &'a [Entry],
    candidate: &Entry,
    today: NaiveDate,
    tz: &Tz,
) -> Option<&'a Entry> {
    if candidate.action != Action::Record || !candidate.enabled {
        return None;
    }
    let range = |e: &Entry| {
        let mut first = today.pred_opt().unwrap_or(today);
        let mut last = today + TimeDelta::days(8);
        if let Some(date) = e.date.filter(|_| e.days.is_once()) {
            first = first.min(date.pred_opt().unwrap_or(date));
            last = last.max(date);
        }
        (first, last)
    };
    entries
        .iter()
        .filter(|e| e.id != candidate.id && e.enabled && e.action == Action::Record)
        .find(|other| {
            let (a_first, a_last) = range(candidate);
            let (b_first, b_last) = range(other);
            let first = a_first.min(b_first);
            let last = a_last.max(b_last);
            let mine = candidate.windows(tz, first, last);
            let theirs = other.windows(tz, first, last);
            mine.iter()
                .any(|(s1, e1)| theirs.iter().any(|(s2, e2)| s1 < e2 && s2 < e1))
        })
}

/// What the controller is to do about an entry that came round
#[derive(Clone, Debug, PartialEq)]
pub enum Decision<Tz: TimeZone> {
    /// Play (and record) the entry's station; `until` ends it
    Start {
        entry: Box<Entry>,
        until: Option<DateTime<Tz>>,
    },
    /// Stop whatever plays
    Stop { entry: Box<Entry> },
    /// Tell the user this
    Notice(String),
}

/// What runs now, as [`run_due`] needs to know
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Running {
    /// A scheduled recording is running: nothing else on the schedule
    /// interrupts it
    pub scheduled_recording: bool,
}

/// Mark the entries due at `now` as run and say what to do. Returns the
/// decisions in the order to carry them out, and whether any entry
/// changed (to be saved).
///
/// - An alarm runs up to [`ALARM_GRACE_SECS`] late; later it is missed.
/// - A recording runs as long as its window is open, recording the rest.
/// - A stop runs up to [`STOP_GRACE_SECS`] late; later it is dropped.
/// - Nothing interrupts a scheduled recording.
/// - A "once" entry switches itself off once it came round.
pub fn run_due<Tz: TimeZone>(
    entries: &mut [Entry],
    now: &DateTime<Tz>,
    running: Running,
) -> (Vec<Decision<Tz>>, bool) {
    let mut due: Vec<(DateTime<Tz>, usize)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.enabled)
        .filter_map(|(i, e)| e.due(now).map(|start| (start, i)))
        .collect();
    if due.is_empty() {
        return (Vec::new(), false);
    }
    // Oldest first, so the newest wins; at the same minute a recording
    // goes first and keeps the rest from interrupting it
    due.sort_by_key(|(start, i)| (start.clone(), entries[*i].action != Action::Record));

    let mut recording = running.scheduled_recording;
    let mut decisions = Vec::new();
    for (start, i) in due {
        let entry = &mut entries[i];
        entry.armed_from = start.timestamp();
        if entry.days.is_once() {
            entry.enabled = false;
        }
        let late = (now.clone() - start.clone()).num_seconds();
        let missed = late <= MISSED_NOTICE_SECS;
        let at = entry.start;
        let title = entry.title();
        match entry.action {
            Action::Play if late > ALARM_GRACE_SECS => {
                if missed {
                    decisions.push(Decision::Notice(format!("Missed {at}: {title}")));
                }
            }
            Action::Play if recording => decisions.push(Decision::Notice(format!(
                "Skipped {at}: {title}, a recording is running"
            ))),
            Action::Play => decisions.push(Decision::Start {
                until: entry.end_after(&start),
                entry: Box::new(entry.clone()),
            }),
            Action::Record => {
                let until = entry.end_after(&start);
                if until.as_ref().is_none_or(|end| end <= now) {
                    if missed {
                        decisions.push(Decision::Notice(format!("Missed {at}: {title}")));
                    }
                } else if recording {
                    decisions.push(Decision::Notice(format!(
                        "Skipped {at}: {title}, a recording is running"
                    )));
                } else {
                    recording = true;
                    decisions.push(Decision::Start {
                        until,
                        entry: Box::new(entry.clone()),
                    });
                }
            }
            Action::Stop if late > STOP_GRACE_SECS => {}
            Action::Stop if recording => decisions.push(Decision::Notice(format!(
                "Skipped {at}: {title}, a recording is running"
            ))),
            Action::Stop => decisions.push(Decision::Stop {
                entry: Box::new(entry.clone()),
            }),
        }
    }
    (decisions, true)
}

/// The next entry to come round after `now`, and when
pub fn next_run<'a, Tz: TimeZone>(
    entries: &'a [Entry],
    now: &DateTime<Tz>,
) -> Option<(DateTime<Tz>, &'a Entry)> {
    entries
        .iter()
        .filter_map(|e| e.next_start(now).map(|start| (start, e)))
        .min_by(|a, b| a.0.cmp(&b.0))
}

/// The entries as saved
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScheduleFile {
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// A wall-clock "when" for a label: "07:00", "Tomorrow 07:00", "Sat 20:00"
pub fn when_label<Tz: TimeZone>(at: &DateTime<Tz>, now: &DateTime<Tz>) -> String
where
    Tz::Offset: fmt::Display,
{
    let time = at.format("%H:%M");
    let days = (at.date_naive() - now.date_naive()).num_days();
    match days {
        0 => time.to_string(),
        1 => format!("Tomorrow {time}"),
        2..=6 => format!("{} {time}", at.format("%a")),
        _ => format!("{} {time}", at.format("%a %-d %b")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDateTime, Offset, Utc};

    /// Athens: UTC+2 in winter, UTC+3 in summer, the clocks changing at
    /// 01:00 UTC on the last Sunday of March and of October
    #[derive(Clone, Copy, Debug)]
    struct Athens;

    fn last_sunday(year: i32, month: u32) -> NaiveDate {
        let next = if month == 12 {
            NaiveDate::from_ymd_opt(year + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(year, month + 1, 1)
        }
        .unwrap();
        let mut day = next.pred_opt().unwrap();
        while day.weekday() != Weekday::Sun {
            day = day.pred_opt().unwrap();
        }
        day
    }

    fn athens_offset(utc: &NaiveDateTime) -> FixedOffset {
        let year = utc.year();
        let change = |month| last_sunday(year, month).and_hms_opt(1, 0, 0).unwrap();
        let summer = *utc >= change(3) && *utc < change(10);
        FixedOffset::east_opt(if summer { 3 * 3600 } else { 2 * 3600 }).unwrap()
    }

    impl TimeZone for Athens {
        type Offset = FixedOffset;

        fn from_offset(_: &FixedOffset) -> Self {
            Athens
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<FixedOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(12, 0, 0).unwrap())
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<FixedOffset> {
            let fits = |off: FixedOffset| athens_offset(&(*local - off)) == off;
            let winter = FixedOffset::east_opt(2 * 3600).unwrap();
            let summer = FixedOffset::east_opt(3 * 3600).unwrap();
            match (fits(winter), fits(summer)) {
                (true, true) => LocalResult::Ambiguous(summer, winter),
                (true, false) => LocalResult::Single(winter),
                (false, true) => LocalResult::Single(summer),
                (false, false) => LocalResult::None,
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            athens_offset(&utc.and_hms_opt(12, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            athens_offset(utc)
        }
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Athens> {
        local_at(&Athens, date(y, m, d), ClockTime::new(h, min).unwrap())
    }

    fn time(text: &str) -> ClockTime {
        ClockTime::parse(text).unwrap()
    }

    fn station(name: &str) -> Option<ScheduledStation> {
        Some(ScheduledStation {
            name: name.into(),
            url: format!("http://example.com/{}", name.replace(' ', "")),
            ..Default::default()
        })
    }

    fn alarm(start: &str, days: Days) -> Entry {
        Entry {
            id: 1,
            enabled: true,
            action: Action::Play,
            station: station("Jazz FM"),
            start: time(start),
            days,
            date: None,
            end: End::Never,
            volume: None,
            fade: false,
            armed_from: 0,
        }
    }

    fn recording(id: u64, start: &str, end: &str, days: Days) -> Entry {
        Entry {
            id,
            action: Action::Record,
            station: station("BBC Radio 3"),
            end: End::At { time: time(end) },
            ..alarm(start, days)
        }
    }

    fn bedtime(start: &str) -> Entry {
        Entry {
            id: 3,
            action: Action::Stop,
            station: None,
            ..alarm(start, Days::EVERY_DAY)
        }
    }

    fn started(decisions: &[Decision<Athens>]) -> Vec<String> {
        decisions
            .iter()
            .map(|d| match d {
                Decision::Start { entry, until } => match until {
                    Some(until) => {
                        format!("start {} until {}", entry.title(), until.format("%H:%M"))
                    }
                    None => format!("start {}", entry.title()),
                },
                Decision::Stop { .. } => "stop".into(),
                Decision::Notice(text) => format!("notice {text}"),
            })
            .collect()
    }

    #[test]
    fn times_of_day_read_and_print() {
        assert_eq!(time("7:05").to_string(), "07:05");
        assert_eq!(time(" 23.59 ").to_string(), "23:59");
        assert_eq!(ClockTime::parse("24:00"), None);
        assert_eq!(ClockTime::parse("7:5"), None);
        assert_eq!(ClockTime::parse("seven"), None);
        assert_eq!(ClockTime::parse("123:00"), None);
    }

    #[test]
    fn days_have_short_labels() {
        assert_eq!(Days::WEEKDAYS.label(), "Weekdays");
        assert_eq!(Days::EVERY_DAY.label(), "Every day");
        assert_eq!(Days::ONCE.label(), "Once");
        let sat = Days::from_flags([false, false, false, false, false, true, false]);
        assert_eq!(sat.label(), "Saturdays");
        let mon_wed = Days::from_flags([true, false, true, false, false, false, false]);
        assert_eq!(mon_wed.label(), "Mon, Wed");
        assert_eq!(Days::from_flags(mon_wed.flags()), mon_wed);
    }

    #[test]
    fn an_entry_saves_and_loads_as_readable_json() {
        let mut entry = recording(7, "20:00", "22:00", Days::WEEKENDS);
        entry.volume = Some(0.4);
        let file = ScheduleFile {
            version: FILE_VERSION,
            entries: vec![
                entry.clone(),
                Entry {
                    days: Days::ONCE,
                    date: Some(date(2026, 10, 3)),
                    ..bedtime("01:00")
                },
            ],
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains(r#""start":"20:00""#), "{json}");
        assert!(json.contains(r#""days":["sat","sun"]"#), "{json}");
        assert!(
            json.contains(r#""end":{"kind":"at","time":"22:00"}"#),
            "{json}"
        );
        assert!(json.contains(r#""date":"2026-10-03""#), "{json}");
        let back: ScheduleFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, file);
    }

    #[test]
    fn a_weekday_alarm_comes_round_on_the_next_weekday() {
        let entry = alarm("07:00", Days::WEEKDAYS);
        // Friday 3 Oct 2026, 08:00: next is Monday
        let now = at(2026, 10, 2, 8, 0);
        assert_eq!(now.weekday(), Weekday::Fri);
        assert_eq!(entry.next_start(&now), Some(at(2026, 10, 5, 7, 0)));
        // Thursday evening: Friday morning
        let now = at(2026, 10, 1, 22, 0);
        assert_eq!(entry.next_start(&now), Some(at(2026, 10, 2, 7, 0)));
    }

    #[test]
    fn an_alarm_runs_once_on_time_and_not_again() {
        let mut entries = vec![alarm("07:00", Days::EVERY_DAY)];
        let before = at(2026, 10, 2, 6, 59);
        entries[0].armed_from = (before - TimeDelta::hours(1)).timestamp();
        let (decisions, changed) = run_due(&mut entries, &before, Running::default());
        assert!(decisions.is_empty() && !changed);

        let now = at(2026, 10, 2, 7, 0);
        let (decisions, changed) = run_due(&mut entries, &now, Running::default());
        assert!(changed);
        assert_eq!(started(&decisions), ["start Play Jazz FM"]);

        let later = now + TimeDelta::seconds(1);
        let (decisions, _) = run_due(&mut entries, &later, Running::default());
        assert!(decisions.is_empty());
        // Tomorrow's is next
        assert_eq!(entries[0].next_start(&later), Some(at(2026, 10, 3, 7, 0)));
    }

    #[test]
    fn a_new_entry_doesnt_run_for_a_time_already_past() {
        let mut entry = alarm("07:00", Days::EVERY_DAY);
        let now = at(2026, 10, 2, 7, 5);
        // Made at 07:05: today's 07:00 is behind it
        entry.armed_from = now.timestamp();
        let mut entries = vec![entry];
        let (decisions, changed) = run_due(&mut entries, &now, Running::default());
        assert!(decisions.is_empty() && !changed);
    }

    #[test]
    fn a_late_alarm_starts_within_ten_minutes_and_is_missed_after() {
        let mut entries = vec![alarm("07:00", Days::EVERY_DAY)];
        let now = at(2026, 10, 2, 7, 9);
        let (decisions, _) = run_due(&mut entries, &now, Running::default());
        assert_eq!(started(&decisions), ["start Play Jazz FM"]);

        let mut entries = vec![alarm("07:00", Days::EVERY_DAY)];
        let now = at(2026, 10, 2, 7, 11);
        let (decisions, changed) = run_due(&mut entries, &now, Running::default());
        assert!(changed);
        assert_eq!(started(&decisions), ["notice Missed 07:00: Play Jazz FM"]);
        // And it isn't reported again
        let (decisions, _) = run_due(&mut entries, &now, Running::default());
        assert!(decisions.is_empty());
    }

    #[test]
    fn a_run_missed_long_ago_passes_without_a_word() {
        let mut entries = vec![alarm("07:00", Days::EVERY_DAY)];
        entries[0].armed_from = at(2026, 9, 1, 0, 0).timestamp();
        let now = at(2026, 10, 2, 23, 0);
        let (decisions, changed) = run_due(&mut entries, &now, Running::default());
        assert!(decisions.is_empty());
        assert!(changed);
    }

    #[test]
    fn a_recording_joins_late_and_is_missed_once_over() {
        let mut entries = vec![recording(2, "20:00", "22:00", Days::EVERY_DAY)];
        let now = at(2026, 10, 3, 20, 30);
        let (decisions, _) = run_due(&mut entries, &now, Running::default());
        assert_eq!(
            started(&decisions),
            ["start Record BBC Radio 3 until 22:00"]
        );

        let mut entries = vec![recording(2, "20:00", "22:00", Days::EVERY_DAY)];
        let now = at(2026, 10, 3, 22, 0);
        let (decisions, _) = run_due(&mut entries, &now, Running::default());
        assert_eq!(
            started(&decisions),
            ["notice Missed 20:00: Record BBC Radio 3"]
        );
    }

    #[test]
    fn a_recording_across_midnight_ends_the_next_day() {
        let entry = recording(2, "23:00", "01:00", Days::EVERY_DAY);
        let start = at(2026, 10, 3, 23, 0);
        assert_eq!(entry.end_after(&start), Some(at(2026, 10, 4, 1, 0)));
        // Still recording at 00:30: yesterday's start is the one due
        let mut entries = vec![entry];
        let now = at(2026, 10, 4, 0, 30);
        let (decisions, _) = run_due(&mut entries, &now, Running::default());
        assert_eq!(
            started(&decisions),
            ["start Record BBC Radio 3 until 01:00"]
        );
    }

    #[test]
    fn nothing_interrupts_a_scheduled_recording() {
        let mut entries = vec![alarm("07:00", Days::EVERY_DAY), bedtime("07:00")];
        let now = at(2026, 10, 2, 7, 0);
        let running = Running {
            scheduled_recording: true,
        };
        let (decisions, _) = run_due(&mut entries, &now, running);
        assert_eq!(
            started(&decisions),
            [
                "notice Skipped 07:00: Play Jazz FM, a recording is running",
                "notice Skipped 07:00: Stop playback, a recording is running"
            ]
        );
    }

    #[test]
    fn a_recording_and_an_alarm_at_the_same_minute_record() {
        let mut entries = vec![
            alarm("20:00", Days::EVERY_DAY),
            recording(2, "20:00", "21:00", Days::EVERY_DAY),
        ];
        let now = at(2026, 10, 2, 20, 0);
        let (decisions, _) = run_due(&mut entries, &now, Running::default());
        assert_eq!(
            started(&decisions),
            [
                "start Record BBC Radio 3 until 21:00",
                "notice Skipped 20:00: Play Jazz FM, a recording is running"
            ]
        );
    }

    #[test]
    fn a_late_bedtime_stop_is_dropped() {
        let mut entries = vec![bedtime("01:00")];
        let (decisions, _) = run_due(&mut entries, &at(2026, 10, 2, 1, 1), Running::default());
        assert_eq!(started(&decisions), ["stop"]);

        let mut entries = vec![bedtime("01:00")];
        let (decisions, changed) =
            run_due(&mut entries, &at(2026, 10, 2, 1, 5), Running::default());
        assert!(decisions.is_empty() && changed);
    }

    #[test]
    fn a_once_entry_switches_itself_off() {
        let entry = Entry {
            days: Days::ONCE,
            ..alarm("18:30", Days::ONCE)
        }
        .tidied(&at(2026, 10, 2, 12, 0));
        assert_eq!(entry.date, Some(date(2026, 10, 2)));
        let mut entries = vec![entry];
        let (decisions, _) = run_due(&mut entries, &at(2026, 10, 2, 18, 30), Running::default());
        assert_eq!(started(&decisions), ["start Play Jazz FM"]);
        assert!(!entries[0].enabled);
        assert_eq!(entries[0].next_start(&at(2026, 10, 2, 19, 0)), None);
    }

    #[test]
    fn a_once_entry_for_a_time_gone_today_is_for_tomorrow() {
        let entry = alarm("07:00", Days::ONCE).tidied(&at(2026, 10, 2, 7, 0));
        assert_eq!(entry.date, Some(date(2026, 10, 3)));
        let entry = alarm("07:00", Days::EVERY_DAY).tidied(&at(2026, 10, 2, 7, 0));
        assert_eq!(entry.date, None);
    }

    #[test]
    fn a_time_skipped_by_the_clocks_going_forward_runs_after_the_gap() {
        // 29 Mar 2026: 03:00 becomes 04:00 in Athens
        let start = at(2026, 3, 29, 3, 30);
        assert_eq!(start.format("%H:%M").to_string(), "04:00");
        let entry = alarm("03:30", Days::EVERY_DAY);
        let mut entries = vec![entry];
        entries[0].armed_from = at(2026, 3, 28, 12, 0).timestamp();
        let (decisions, _) = run_due(&mut entries, &start, Running::default());
        assert_eq!(started(&decisions), ["start Play Jazz FM"]);
    }

    #[test]
    fn a_time_that_happens_twice_runs_once() {
        // 25 Oct 2026: 04:00 becomes 03:00 again, so 03:30 comes twice
        let first = at(2026, 10, 25, 3, 30);
        assert_eq!(first.offset().fix().local_minus_utc(), 3 * 3600);
        let mut entries = vec![alarm("03:30", Days::EVERY_DAY)];
        entries[0].armed_from = at(2026, 10, 24, 12, 0).timestamp();
        let (decisions, _) = run_due(&mut entries, &first, Running::default());
        assert_eq!(decisions.len(), 1);
        let second = first + TimeDelta::hours(1);
        assert_eq!(second.format("%H:%M").to_string(), "03:30");
        let (decisions, _) = run_due(&mut entries, &second, Running::default());
        assert!(decisions.is_empty());
    }

    #[test]
    fn a_play_for_two_hours_across_the_clock_change_lasts_two_hours() {
        let mut entry = alarm("02:00", Days::EVERY_DAY);
        entry.end = End::After { minutes: 120 };
        let start = at(2026, 10, 25, 2, 0);
        let end = entry.end_after(&start).unwrap();
        assert_eq!((end - start).num_minutes(), 120);
        assert_eq!(end.with_timezone(&Utc).format("%H:%M").to_string(), "01:00");
    }

    #[test]
    fn two_recordings_at_the_same_time_clash() {
        let saturday = Days::from_flags([false, false, false, false, false, true, false]);
        let show = recording(1, "20:00", "22:00", saturday);
        let late = recording(2, "21:30", "23:00", Days::WEEKENDS);
        let sunday_only = recording(3, "21:30", "23:00", Days::ONCE);
        let today = date(2026, 10, 2);
        let entries = vec![show.clone()];
        assert_eq!(
            clashing_recording(&entries, &late, today, &Athens).map(|e| e.id),
            Some(1)
        );
        // Once, on a Sunday: no clash
        let sunday = Entry {
            date: Some(date(2026, 10, 4)),
            ..sunday_only.clone()
        };
        assert!(clashing_recording(&entries, &sunday, today, &Athens).is_none());
        // Once, on a Saturday two weeks off: clash
        let saturday_later = Entry {
            date: Some(date(2026, 10, 17)),
            ..sunday_only
        };
        assert!(clashing_recording(&entries, &saturday_later, today, &Athens).is_some());
        // Back to back is fine, and an entry doesn't clash with itself
        let next = recording(4, "22:00", "23:00", saturday);
        assert!(clashing_recording(&entries, &next, today, &Athens).is_none());
        assert!(clashing_recording(&entries, &show, today, &Athens).is_none());
        // A recording that's off doesn't count
        let off = vec![Entry {
            enabled: false,
            ..show
        }];
        assert!(clashing_recording(&off, &late, today, &Athens).is_none());
    }

    #[test]
    fn entries_say_what_is_wrong_with_them() {
        let mut entry = recording(1, "20:00", "22:00", Days::WEEKENDS);
        assert_eq!(entry.check(), Ok(()));
        entry.end = End::Never;
        assert_eq!(entry.check(), Err("A recording needs an end".into()));
        entry.end = End::After { minutes: 0 };
        assert!(entry.check().is_err());
        entry.end = End::At {
            time: time("20:00"),
        };
        assert!(entry.check().is_err());
        entry.end = End::After { minutes: 90 };
        entry.station = None;
        assert_eq!(entry.check(), Err("Pick a station".into()));
        let stop = bedtime("01:00");
        assert_eq!(stop.check(), Ok(()));
        let once = Entry {
            days: Days::ONCE,
            ..stop
        };
        assert!(once.check().is_err());
    }

    #[test]
    fn the_next_run_is_the_soonest_enabled_entry() {
        let mut off = alarm("06:00", Days::EVERY_DAY);
        off.enabled = false;
        let show = recording(2, "20:00", "22:00", Days::EVERY_DAY);
        let entries = vec![off, alarm("07:00", Days::EVERY_DAY), show];
        let now = at(2026, 10, 2, 12, 0);
        let (when, entry) = next_run(&entries, &now).unwrap();
        assert_eq!(when, at(2026, 10, 2, 20, 0));
        assert_eq!(entry.id, 2);
        assert_eq!(when_label(&when, &now), "20:00");
        let (when, _) = next_run(&entries, &at(2026, 10, 2, 23, 0)).unwrap();
        assert_eq!(when_label(&when, &at(2026, 10, 2, 23, 0)), "Tomorrow 07:00");
    }

    #[test]
    fn details_read_like_the_list() {
        let mut entry = alarm("07:00", Days::WEEKDAYS);
        entry.end = End::After { minutes: 90 };
        entry.fade = true;
        entry.volume = Some(0.4);
        assert_eq!(
            entry.details(),
            "Weekdays · for 1 h 30 min · volume 40% · fade in"
        );
        let mut stop = bedtime("01:00");
        stop.fade = true;
        assert_eq!(stop.details(), "Every day · fade out");
    }
}
