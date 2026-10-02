//! A save run on a thread of its own, a moment after it is asked for
//!
//! Asking again before it ran joins the save already due, so changes that
//! come close together are written once, and the thread asking never waits
//! on the disk.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

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

    /// Save soon. A save already due runs at its time and takes this
    /// change along.
    pub fn request(&self) {
        let mut pending = self.shared.lock();
        if pending.due.is_none() {
            pending.due = Some(Instant::now() + self.delay);
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
