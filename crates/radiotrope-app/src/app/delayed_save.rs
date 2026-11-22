//! A save run on a thread of its own, a moment after it is asked for
//!
//! Asking again before it ran joins the save already due, so changes that
//! come close together are written once, and the thread asking never waits
//! on the disk.
//!
//! A save that can fail is started with
//! [`start_reporting`](DelayedSave::start_reporting): the failure is
//! logged, kept in [`SaveErrors`] for the window to show, and the save is
//! tried again until it works.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// The first retry of a failed save comes this long after it; each one
/// after that takes twice as long, up to [`SAVE_RETRY_LONGEST`]
pub const SAVE_RETRY: Duration = Duration::from_secs(5);

/// The longest a failed save waits for its next try
pub const SAVE_RETRY_LONGEST: Duration = Duration::from_secs(60);

/// How a save that worked left things
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Saved {
    /// Nothing is left to save
    All,
    /// Something changed while the disk was written: save again
    More,
}

/// What the window shows of a failed save: `"Can't save favorites:
/// permission denied"`, or nothing
type Show = Box<dyn Fn(String) + Send + Sync>;

#[derive(Default)]
struct Errors {
    /// Why the last save of each kind ("favorites", "settings") failed.
    /// One that worked since isn't here.
    reasons: Mutex<BTreeMap<&'static str, String>>,
    show: OnceLock<Show>,
}

/// The saves that failed and haven't worked since. Cloning it clones the
/// handle.
#[derive(Clone, Default)]
pub struct SaveErrors {
    inner: Arc<Errors>,
}

impl SaveErrors {
    /// The one the window shows
    pub fn shared() -> &'static SaveErrors {
        static SHARED: OnceLock<SaveErrors> = OnceLock::new();
        SHARED.get_or_init(SaveErrors::default)
    }

    /// Note how a save of `what` went: `Some(reason)` when it failed,
    /// `None` when it worked. A change is logged and shown.
    pub fn set(&self, what: &'static str, reason: Option<String>) {
        let text = {
            let mut reasons = self.inner.reasons.lock().unwrap_or_else(|e| e.into_inner());
            match reason {
                Some(reason) => {
                    if reasons.get(what) == Some(&reason) {
                        return;
                    }
                    eprintln!("Can't save {what}: {reason}");
                    reasons.insert(what, reason);
                }
                None => {
                    if reasons.remove(what).is_none() {
                        return;
                    }
                    eprintln!("Saving {what} works again");
                }
            }
            Self::text_of(&reasons)
        };
        if let Some(show) = self.inner.show.get() {
            show(text);
        }
    }

    /// What to show: the first failed save, or nothing
    pub fn text(&self) -> String {
        Self::text_of(&self.inner.reasons.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn text_of(reasons: &BTreeMap<&'static str, String>) -> String {
        reasons
            .iter()
            .next()
            .map(|(what, reason)| format!("Can't save {what}: {reason}"))
            .unwrap_or_default()
    }

    /// Call `show` with the text now, and again each time it changes (from
    /// any thread). The first caller's `show` stays.
    pub fn show_with(&self, show: impl Fn(String) + Send + Sync + 'static) {
        if self.inner.show.set(Box::new(show)).is_ok() {
            if let Some(show) = self.inner.show.get() {
                show(self.text());
            }
        }
    }
}

#[derive(Default)]
struct Pending {
    /// When the save asked for is due
    due: Option<Instant>,
    /// The save is being run now
    running: bool,
}

struct Shared {
    pending: Mutex<Pending>,
    changed: Condvar,
    save: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run the save with `pending` let go, then hand it back
    fn run<'a>(&'a self, mut pending: MutexGuard<'a, Pending>) -> MutexGuard<'a, Pending> {
        pending.due = None;
        pending.running = true;
        drop(pending);
        (self.save)();
        let mut pending = self.lock();
        pending.running = false;
        self.changed.notify_all();
        pending
    }
}

#[derive(Clone)]
pub struct DelayedSave {
    shared: Arc<Shared>,
    delay: Duration,
}

impl DelayedSave {
    /// Run `save` on a thread named `name`, `delay` after it is asked for.
    /// When the thread can't be started, a save is only run by
    /// [`flush`](Self::flush).
    pub fn start(name: &str, delay: Duration, save: impl Fn() + Send + Sync + 'static) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::default(),
            changed: Condvar::new(),
            save: Box::new(save),
        });
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut pending = worker.lock();
                loop {
                    let due = pending.due;
                    pending = match due {
                        Some(due) if Instant::now() >= due => worker.run(pending),
                        Some(due) => {
                            let wait = due.saturating_duration_since(Instant::now());
                            worker
                                .changed
                                .wait_timeout(pending, wait)
                                .unwrap_or_else(|e| e.into_inner())
                                .0
                        }
                        None => worker
                            .changed
                            .wait(pending)
                            .unwrap_or_else(|e| e.into_inner()),
                    };
                }
            });
        if let Err(e) = spawned {
            eprintln!("Failed to start the {name} thread: {e}");
        }
        Self { shared, delay }
    }

    /// [`start`](Self::start) for a `save` that can fail. It returns how
    /// it went, or why it failed in plain words ("permission denied").
    /// A failure is noted in `errors` as a save of `what` ("favorites")
    /// until a save works, and the save is tried again: after `retry`, then
    /// after twice that, up to [`SAVE_RETRY_LONGEST`].
    pub fn start_reporting(
        name: &str,
        delay: Duration,
        what: &'static str,
        errors: SaveErrors,
        retry: Duration,
        save: impl Fn() -> Result<Saved, String> + Send + Sync + 'static,
    ) -> Self {
        let this: Arc<OnceLock<DelayedSave>> = Arc::default();
        let failures = AtomicU32::new(0);
        let saver = Self::start(name, delay, {
            let this = this.clone();
            move || match save() {
                Ok(saved) => {
                    failures.store(0, Ordering::Relaxed);
                    errors.set(what, None);
                    if let (Saved::More, Some(saver)) = (saved, this.get()) {
                        saver.request();
                    }
                }
                Err(reason) => {
                    let failed = failures.fetch_add(1, Ordering::Relaxed);
                    errors.set(what, Some(reason));
                    if let Some(saver) = this.get() {
                        let wait = retry.saturating_mul(1 << failed.min(8));
                        saver.request_in(wait.min(SAVE_RETRY_LONGEST.max(retry)));
                    }
                }
            }
        });
        let _ = this.set(saver.clone());
        saver
    }

    /// Save soon. A save already due runs at its time and takes this
    /// change along.
    pub fn request(&self) {
        self.request_in(self.delay);
    }

    /// Save at once (still on the saving thread): for a change the user
    /// just made
    pub fn request_now(&self) {
        self.request_in(Duration::ZERO);
    }

    /// Save in `delay`, or when a save is due before that, then
    fn request_in(&self, delay: Duration) {
        let due = Instant::now() + delay;
        let mut pending = self.shared.lock();
        if pending.due.is_none_or(|already| due < already) {
            pending.due = Some(due);
            self.shared.changed.notify_all();
        }
    }

    /// Run a save that is due now, on this thread, after one being run
    /// has finished (at exit)
    pub fn flush(&self) {
        let mut pending = self.shared.lock();
        while pending.running {
            pending = self
                .shared
                .changed
                .wait(pending)
                .unwrap_or_else(|e| e.into_inner());
        }
        if pending.due.is_some() {
            drop(self.shared.run(pending));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn counting(delay: Duration) -> (DelayedSave, Arc<AtomicUsize>) {
        let saves = Arc::new(AtomicUsize::new(0));
        let counter = saves.clone();
        let saver = DelayedSave::start("test-save", delay, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        (saver, saves)
    }

    #[test]
    fn requests_close_together_are_saved_once_after_the_delay() {
        let (saver, saves) = counting(Duration::from_millis(200));
        saver.request();
        saver.request();
        std::thread::sleep(Duration::from_millis(50));
        saver.request();
        assert_eq!(saves.load(Ordering::SeqCst), 0);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(saves.load(Ordering::SeqCst), 1);

        // A later request is a new save
        saver.request();
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(saves.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn flush_saves_what_is_due_at_once_and_only_then() {
        let (saver, saves) = counting(Duration::from_secs(60));
        saver.flush();
        assert_eq!(saves.load(Ordering::SeqCst), 0);
        saver.request();
        saver.flush();
        assert_eq!(saves.load(Ordering::SeqCst), 1);
        // Nothing is left for the thread to save later
        saver.flush();
        assert_eq!(saves.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_save_asked_for_now_does_not_wait_for_the_delay() {
        let (saver, saves) = counting(Duration::from_secs(60));
        saver.request();
        saver.request_now();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(saves.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_save_is_shown_and_tried_again_until_it_works() {
        let errors = SaveErrors::default();
        let shown = Arc::new(Mutex::new(Vec::new()));
        errors.show_with({
            let shown = shown.clone();
            move |text| shown.lock().unwrap().push(text)
        });
        let tries = Arc::new(AtomicUsize::new(0));
        let saver = DelayedSave::start_reporting(
            "test-save",
            Duration::from_secs(60),
            "favorites",
            errors.clone(),
            Duration::from_millis(20),
            {
                let tries = tries.clone();
                // The disk is full for the first three tries
                move || match tries.fetch_add(1, Ordering::SeqCst) {
                    0..3 => Err("no space left on device".to_string()),
                    _ => Ok(Saved::All),
                }
            },
        );
        saver.request_now();
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            errors.text(),
            "Can't save favorites: no space left on device"
        );

        // 20 + 40 + 80 ms of retries, asked for by nobody
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(tries.load(Ordering::SeqCst), 4);
        assert_eq!(errors.text(), "");
        assert_eq!(
            *shown.lock().unwrap(),
            ["", "Can't save favorites: no space left on device", ""],
            "shown once, though it failed three times, and cleared"
        );
    }

    #[test]
    fn the_first_of_two_failed_saves_is_shown_until_both_work() {
        let errors = SaveErrors::default();
        errors.set("settings", Some("permission denied".into()));
        errors.set("favorites", Some("read-only file system".into()));
        assert_eq!(errors.text(), "Can't save favorites: read-only file system");
        errors.set("favorites", None);
        assert_eq!(errors.text(), "Can't save settings: permission denied");
        errors.set("settings", None);
        assert_eq!(errors.text(), "");
    }

    #[test]
    fn a_save_that_left_more_to_save_runs_again() {
        let tries = Arc::new(AtomicUsize::new(0));
        let saver = DelayedSave::start_reporting(
            "test-save",
            Duration::from_millis(20),
            "favorites",
            SaveErrors::default(),
            SAVE_RETRY,
            {
                let tries = tries.clone();
                move || match tries.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok(Saved::More),
                    _ => Ok(Saved::All),
                }
            },
        );
        saver.request_now();
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(tries.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn flush_waits_for_a_save_being_run() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let done = Arc::new(AtomicUsize::new(0));
        let finished = done.clone();
        let saver = DelayedSave::start("test-save", Duration::ZERO, move || {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(200));
            finished.fetch_add(1, Ordering::SeqCst);
        });
        saver.request();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        saver.flush();
        assert_eq!(done.load(Ordering::SeqCst), 1);
    }
}
